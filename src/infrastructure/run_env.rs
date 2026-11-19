//! The repository's `dagq.toml` (ADR-0023 decision 3): `[run.env]` holds
//! environment variables every run gets, in its worker workspace and in the
//! verification commands; its values are strings in which
//! `${DAGQ_QUEUE_DIR}` and `${DAGQ_RUN_DIR}` are expanded. `[stall]` holds
//! the thresholds of the stalled-session checks in seconds (ADR-0043
//! decision 4). `[conflicts]` holds the thresholds of the
//! `conflict_hotspot` alert of `stats` (goal 31). `[recheck]` holds the
//! `command` the landing recheck runs on main's tree with a waiting run
//! merged in (ADR-0068 decision 2), and the `paths` a run's diff must touch
//! for it to run (ADR-t2032-1). `[disk]` holds how much free disk
//! space a claim and a landing need (ADR-0047 decision 44, task 377).
//! `[resume]` holds the limit of a run's conflict-only attempts (ADR-0047
//! decision 24). `[exit]` held the retries of a `/exit` the session held
//! back and the wait after each (ADR-0047 decision 25): since task 1437 it
//! is still checked when the file is parsed and is otherwise ignored. `[worker.trial]` turns on the limited trial of the worker's model
//! (ADR-0079 decision 4). `[roles.<role>]` holds the provider, model and effort
//! of a session other than the worker's (ADR-0079 decision 7, ADR-t1063-1
//! decision 1), and `runtime`, where a role runs: `[roles.worker]` and
//! `[roles.verification]` hold only it (goal 38,
//! [`crate::domain::container_runtime::RuntimeConfig`]). `[supervisor]`
//! holds `parallel`, `max_waiting`, `runtime_planners` and `claim_spacing`
//! of a supervisor started without the flags (task 698, task 941,
//! ADR-t1479-1). `[areas]` maps the
//! areas `stats` and `kpi` split the landed runs by to globs (ADR-t980-1),
//! and `[tasks] changes` names the set of changes a task declares one of,
//! `[tasks] title_prefixes` the prefixes a task's title starts with,
//! `[tasks] related_docs` the globs of the documents `lint` looks in for
//! its hints (ADR-t3321-1).
//! `[goals] tags` names the set of tags a goal's tags are taken from
//! (ADR-t1639-1 decision 6).
//! `[e2e] paths` names, as globs, the paths whose change requires `e2e` of
//! a run (ADR-t963-1 decision 2). `[broker]` and `[broker.package]`, the
//! settings of the removed resource broker, are accepted and not read
//! (ADR-t2125-1).
//! `[headless] wrapper`, which chose where an earlier binary ran a headless
//! session's wrapper (in a cmux workspace or as a background process,
//! ADR-t1404-1), is accepted and read only to warn that it is ignored: every
//! wrapper runs in the background (ADR-t1433-3 decision 1).
//! `[provider_fallback] workers` turns off a worker's move off a provider
//! it cannot use, and `jobs` that of a headless job whose role names its
//! provider (ADR-t1857-1).
//! `[fresh_session] peak_context_above` starts a worker's send-back or
//! resume in a new session past a context of that many tokens
//! (ADR-t2080-1).
//! `[review.jobs]` sets the timeout of each kind of job of a run's review
//! (ADR-t1895-1 decision 1) and how many of the reviews' agent jobs run at
//! once. `[review.programs.<name>]` names a program
//! review, what it runs and the globs that require it (ADR-t1895-2); the
//! review reads it from the landing branch's commit.
//! `[egress] registries` names the package registries the queue broker
//! opens a tunnel to (ADR-t2257-1 decision 3, [`EGRESS_TABLE`]).
//! `[container.image]` names the project's Containerfile and the files it
//! copies, on which the container runtime's image is built (ADR-t2257-1
//! decision 2).
//! `[container.build_output]` names the directory of a run's container its
//! build output volume is mounted at ([`CONTAINER_BUILD_OUTPUT_TABLE`]).
//! `[container.worker_cache]` names the directories of a worker's container
//! its queue's worker cache is mounted at ([`CONTAINER_WORKER_CACHE_TABLE`]).
//! `[toolchains.<name>]` declares a toolchain profile the repository uses
//! and the paths it applies to (ADR-t2268-1, [`TOOLCHAINS_PREFIX`]).
//! `[verify_marks.<name>]` declares a mark of the project's own on its
//! verification's output (ADR-t2268-1 decision 2, [`VERIFY_MARKS_PREFIX`]).
//! `[local_checks]` and `[local_checks.gates.<name>]` declare the tests and
//! the worker's local gates the review judges by a run's diff
//! (ADR-t2589-1 decision 6, [`LOCAL_GATES_PREFIX`]).
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
        ChangeSet, GoalTag, ReviseLimit, TagSet, TaskChange, TitlePrefixes,
        actor_model::{ModelRole, RoleModel, RoleModels, check_effort},
        agent_eval::round::{EvalConfig, ProviderCost},
        areas::AreaMap,
        background_wrapper::HeadlessWrapper,
        ci_watch::{CiWatchConfig, DEFAULT_INTERVAL_SECS, MIN_INTERVAL_SECS},
        container_image::{ProjectImage, repository_path_problem},
        container_runtime::{RUNTIME_KEY, Runtime, RuntimeConfig, RuntimeTarget, refuse_container},
        disk::{DiskConfig, DiskEstimate, WaitingBuildCleanup},
        exit::ExitConfig,
        fresh_session::FreshSessionConfig,
        headless_job::ReviewJobs,
        kpi::KpiSettings,
        landing_branch::RepositoryConfig,
        landing_verification::{self, LandingVerification},
        light_slots::LightChanges,
        local_config::{LocalLayerRead, LocalLayerState},
        local_gates::{GateConfig, LocalGate, LocalGates},
        provider_switch::ProviderFallback,
        queue_broker::RegistryHost,
        recheck::RecheckConfig,
        replan_detect::ReplanConfig,
        resume::ResumeConfig,
        retention::RetentionConfig,
        review_builtins::{self, BuiltinAgent, BuiltinSetting},
        review_programs::{
            self, BuiltinProgram, BuiltinProgramSetting, ReviewProgram, ReviewPrograms,
        },
        review_reason::CodePaths,
        review_subagents::{self, ReviewAgents, ReviewSubagent},
        run_env::{RunEnvCheck, RunEnvProgram},
        scope::{dedup_globs, validate_path_globs},
        slot_limits::SupervisorConfig,
        stall::StallConfig,
        stats::ConflictConfig,
        throughput_review::ThroughputReviewConfig,
        toolchain::{ProfileRegistry, ToolchainDeclaration, Toolchains},
        verification_limit::{self, VerificationConfig},
        verify_marks::{LinePattern, MarkClass, ProjectMark},
        worker_model::{ProviderTrial, WorkerTrial},
    },
    infrastructure::kpi_config::KpiTables,
    infrastructure::recurring_config::{RECURRING_TABLE, RecurringTables},
    infrastructure::telemetry_config::{COMMANDS_TABLE, TELEMETRY_TABLE, TelemetryTables},
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
/// `[retention]`: how long the directories of ended planners, plan
/// reviews and observations stay, and how many backups (ADR-t697-1); its
/// keys are on [`RetentionConfig`].
const RETENTION_TABLE: &str = "retention";
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
/// `[worker.provider_trial]`: the trial of the provider pair of the worker
/// and its review (ADR-t4005-1).
const PROVIDER_TRIAL_TABLE: &str = "worker.provider_trial";
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
/// `[tasks]`: `changes`, the repository's set of changes (ADR-t980-1),
/// `title_prefixes`, the prefixes a task's title starts with, and
/// `related_docs`, the globs of `lint`'s related documents (ADR-t3321-1).
const TASKS_TABLE: &str = "tasks";
/// The keys of `[tasks]`.
const TASKS_CHANGES: &str = "changes";
const TASKS_TITLE_PREFIXES: &str = "title_prefixes";
const TASKS_RELATED_DOCS: &str = "related_docs";
/// `[goals]`: `tags`, the repository's set of goal tags (ADR-t1639-1
/// decision 6).
const GOALS_TABLE: &str = "goals";
/// The keys of `[goals]`.
const GOALS_TAGS: &str = "tags";
/// The Spike/replanning cycle limit of a goal, read only by submit.
const GOALS_SPIKE_CYCLES: &str = "max_spike_cycles";
/// `[e2e]` accepts `paths`, the repository-relative globs whose change
/// requires e2e (ADR-t963-1 decision 2; [`crate::domain::scope`]),
/// `command`, a non-blank single-line shell string, and `timeout_secs`,
/// a positive integer of seconds requiring `command`. Without a timeout,
/// [`crate::application::install::E2E_TIMEOUT`] applies. Repeated keys and
/// unknown keys are errors; repeated path globs are kept once.
const E2E_TABLE: &str = "e2e";
const E2E_PATHS: &str = "paths";
/// The tables of the removed resource broker (ADR-t2125-1): accepted
/// without looking into them, however many times they come, so a
/// repository whose `dagq.toml` still has them is not stopped by them.
/// Nothing reads them.
const RETIRED_TABLES: [&str; 2] = ["broker", "broker.package"];
/// What [`parse_config`] calls the current table while in one of
/// [`RETIRED_TABLES`].
const RETIRED_TABLE: &str = "broker";
/// `[review.subagents.<agent>]`: the globs that make a review's subagent
/// required (ADR-t1453-1 decision 1), one table per agent.
/// A custom agent's name is kebab-case and names its directory under
/// `.dagq/agents/<agent>/AGENT.md`. A `builtin:` name instead selects
/// a built-in: its optional paths replace the default paths, omitted
/// settings retain defaults. Syntax errors fail the whole configuration.
/// Missing definitions and names used by multiple roles are reported in
/// doctor's `agents.errors`; the review handles them as definition errors.
const REVIEW_SUBAGENTS_PREFIX: &str = "review.subagents.";
/// What [`parse_config`] calls the current table while in a
/// `[review.subagents.*]`.
const REVIEW_SUBAGENTS_TABLE: &str = "review.subagents";
/// The required key of `[review.subagents.<agent>]`.
const REVIEW_SUBAGENT_PATHS: &str = "paths";
/// The optional key of `[review.subagents.<agent>]`: the provider of the
/// agent's job ([`ReviewSubagent::provider`]).
const REVIEW_SUBAGENT_PROVIDER: &str = "provider";
/// The optional keys of `[review.subagents.<agent>]` and of a built-in's
/// table: the model and effort of the agent's job
/// ([`ReviewSubagent::model`], [`ReviewSubagent::effort`]), checked
/// against the provider it starts on when the review reads them.
const REVIEW_SUBAGENT_MODEL: &str = "model";
const REVIEW_SUBAGENT_EFFORT: &str = "effort";
/// The optional key of `[review.subagents."builtin:<name>"]`: whether the
/// built-in runs ([`BuiltinSetting::enabled`]).
const REVIEW_SUBAGENT_ENABLED: &str = "enabled";
/// What [`parse_config`] calls the current table while in a
/// `[review.subagents."builtin:<name>"]`; its keys are `enabled` and the
/// optional `paths`, `provider`, `model` and `effort` (`enabled` only for
/// [`review_builtins::ACCEPTANCE`]).
const REVIEW_BUILTIN_TABLE: &str = "review.subagents.builtin";
/// `[review.programs.<name>]`: a program review (ADR-t1895-2), one table
/// per program; its keys are [`REVIEW_PROGRAM_KEYS`].
const REVIEW_PROGRAMS_PREFIX: &str = "review.programs.";
/// What [`parse_config`] calls the current table while in a
/// `[review.programs.*]`.
const REVIEW_PROGRAMS_TABLE: &str = review_programs::SECTION;
/// The keys of `[review.programs.<name>]`: `script` (a repository path,
/// required) with an optional `args` array, `paths` (the globs, required)
/// and an optional `timeout_secs`; what each means is on
/// [`ReviewProgram`].
const REVIEW_PROGRAM_KEYS: [&str; 4] = ["script", "args", "paths", "timeout_secs"];
/// What [`parse_config`] calls the current table while in a
/// `[review.programs."builtin:<name>"]`; its keys are the optional
/// `enabled` and `paths` ([`BuiltinProgramSetting`]). `script` and `args`
/// are mistakes: what a built-in runs is the binary's.
const REVIEW_BUILTIN_PROGRAM_TABLE: &str = "review.programs.builtin";
/// `[headless]`: `wrapper`, where a headless session's wrapper runs
/// (ADR-t1404-1 decision 7).
/// Accepts `wrapper = "workspace"` or `"background"`, but neither changes
/// workers or runtime planners: both always run in the background.
/// A workspace value warns once per supervisor process that it has no effect.
const HEADLESS_TABLE: &str = "headless";
/// The one key of `[headless]`.
const HEADLESS_WRAPPER: &str = "wrapper";
/// `[provider_fallback]`: whether a worker and a job move off a provider
/// they cannot use (ADR-t1857-1).
const PROVIDER_FALLBACK_TABLE: &str = "provider_fallback";
/// `[fresh_session]`: the context past which a worker's send-back or
/// resume starts a new session (ADR-t2080-1); its key is on
/// [`FreshSessionConfig`].
const FRESH_SESSION_TABLE: &str = "fresh_session";
/// `[throughput_review]`: what each throughput review gathers and lets
/// its job read; its key, default and meaning are on
/// [`ThroughputReviewConfig`]. It is not `[roles.throughput_review]`, the
/// review's provider. A binary older than this table refuses it, so a
/// repository adds it only once the binary that runs its queue reads it.
const THROUGHPUT_REVIEW_TABLE: &str = "throughput_review";
/// `[replan]`: whether the runtime detects a long-running task and
/// diagnoses the replanning operations, and its limits
/// (docs/plans/task-replanning.md); its keys, defaults and meaning are on
/// [`ReplanConfig`]. A binary older than this table refuses it, so a
/// repository adds it only once the binary that runs its queue reads it.
const REPLAN_TABLE: &str = "replan";
/// `[review.jobs]`: the timeout of each kind of job of a run's review
/// (ADR-t1895-1 decision 1) and how many agent jobs of the reviews run at
/// once; its keys are on [`ReviewJobs`].
const REVIEW_JOBS_TABLE: &str = "review.jobs";
/// `[review.code]`: `paths`, the repository's code paths, which tell an
/// `acceptance_unmet` of a run's review on code from one elsewhere past
/// the revise limit (ADR-t2599-1); its format, default and meaning are on
/// [`CodePaths`].
const REVIEW_CODE_TABLE: &str = "review.code";
/// The one key of `[review.code]`.
const REVIEW_CODE_PATHS: &str = "paths";
/// `[review]`: `revise_limit`, the run's revise limit, a whole number of
/// at least 1 that each review attempt reads from the landing branch's
/// commit; its default and meaning are on [`ReviseLimit`].
const REVIEW_TABLE: &str = "review";
/// The one key of `[review]`.
const REVIEW_REVISE_LIMIT: &str = "revise_limit";
/// `[ci_watch]`: the landing branch's CI the supervisor watches
/// (ADR-t1920-1). `workflow` (a non-blank string) is required; `branch`
/// (a branch name without `refs/heads/`), `interval_secs` (at least
/// [`MIN_INTERVAL_SECS`]), `junit_artifacts` (an array of distinct
/// non-blank globs), `required_jobs` (an array of distinct non-blank
/// job names, ADR-t2034-1) and `scheduled_workflows` (an array of distinct
/// non-blank workflows, ADR-t3807-1) are optional; what each means is on
/// [`CiWatchConfig`].
const CI_WATCH_TABLE: &str = "ci_watch";
/// `[landing_verification]`: the command `integrate` runs in place of
/// some of a task's (ADR-t1925-1 decision 4).
const LANDING_VERIFICATION_TABLE: &str = landing_verification::TABLE;

/// The landing table while it is being read; the replacement pair is
/// required together, while the build mode can stand alone.
#[derive(Default)]
struct LandingTable {
    line: usize,
    replaces: Option<Vec<String>>,
    command: Option<String>,
    non_incremental: Option<bool>,
}

/// `[verification]`: the time limit of one verification command
/// (`command_timeout_secs`, a positive whole number of seconds); what it
/// bounds and its default are on [`VerificationConfig`]. A binary older
/// than this table refuses it, so a repository adds it only once the
/// binary that runs its queue reads it.
const VERIFICATION_TABLE: &str = verification_limit::TABLE;
/// `[eval]`: the limits of a round of the eval of an agent (ADR-t1728-1
/// decisions 8 and 9); its keys are on [`EvalConfig`].
const EVAL_TABLE: &str = "eval";
/// `[eval.providers.<provider>]`: one provider's estimate and prices of a
/// run, its keys on [`ProviderCost`]; its heading declares the provider
/// ([`EvalConfig::declared_providers`]).
const EVAL_PROVIDERS_PREFIX: &str = "eval.providers.";
/// What [`parse_config`] calls the current table while in a
/// `[eval.providers.*]`.
const EVAL_PROVIDER_TABLE: &str = "eval.providers";
/// `[egress]`: `registries` ([`EGRESS_REGISTRIES`]), the package
/// registries a run in a container reaches through the queue broker
/// (ADR-t2257-1 decision 3).
pub const EGRESS_TABLE: &str = "egress";
/// The one key of `[egress]`: a one-line array of the hosts of the package
/// registries the queue broker passes a `CONNECT` to, each `"<host>"`
/// (port 443) or `"<host>:<port>"`
/// ([`crate::domain::queue_broker::RegistryHost`]), with why each is
/// needed in a comment above it. The default, without the table or the
/// key, is none: no registry is reached, and the runtime adds none of its
/// own. The broker reads it from the `dagq.toml` of the main checkout the
/// queue is bound to, each time a `CONNECT` comes, never from a run's
/// clone. A host of the repository's upstream git remotes (by the name in
/// their `url` and `pushurl`) is refused even when named here; so is
/// `api.anthropic.com`. Names are matched exactly: another name or an
/// address of the same forge is not recognized, so name none. A binary
/// older than this key refuses the unknown table `[egress]`, so a
/// repository adds it only once the binary that runs its queue reads it.
pub const EGRESS_REGISTRIES: &str = "registries";
/// `[container.image]`: the project's image on the container runtime's
/// base (ADR-t2257-1 decision 2); its keys and their meaning are on
/// [`ProjectImage`].
const CONTAINER_IMAGE_TABLE: &str = "container.image";
/// `[container.build_output]`: where a worker's container mounts its run's
/// build output (ADR-t2173-1 decision 2), a named volume of the dagq
/// machine that only the run's containers mount and that is removed at the
/// run's end and its landing. Its one key, `dir` (a string), is the
/// directory in the container: absolute, not `/`, without `,`, `:` or
/// `..`. Without the table no such volume is mounted, which is the
/// default: the runtime knows no build tool, so the project points its
/// tool there through `[run.env]` (for cargo, `CARGO_TARGET_DIR`). Build
/// output on the clone (a mount of the host) is where links were seen to
/// fail. A binary older than this table refuses it, so a repository adds
/// it only once the binary that runs its queue reads it.
pub const CONTAINER_BUILD_OUTPUT_TABLE: &str = "container.build_output";
/// The key of [`CONTAINER_BUILD_OUTPUT_TABLE`].
const CONTAINER_BUILD_OUTPUT_DIR: &str = "dir";
/// `[container.worker_cache]`: the directories of a worker's container
/// (resumes included) its queue's worker cache is mounted at (ADR-t2257-1
/// decision 5, ADR-t2173-1 decision 3). Its one key, `dirs` (an array of
/// strings), lists directories in the container: each absolute, not `/`,
/// without `,`, `:` or `..`, none twice. Each is mounted from a named
/// volume of the dagq machine that the queue's workers and resumes share
/// and no other queue's container nor any verification's mounts
/// ([`QueueKey::worker_cache_volume_of`]); the host never reads it, so it
/// holds a cache no one trusts but the workers. Without the table, or with
/// an empty list, nothing is mounted, which is the default: the runtime
/// knows no tool's cache, so the project names the directories (a package
/// manager's cache, a compiler cache's directory) and points its tools
/// there through `[run.env]` or the image. A binary older than this table
/// refuses it, so a repository adds it only once the binary that runs its
/// queue reads it.
///
/// [`QueueKey::worker_cache_volume_of`]: crate::domain::podman_machine::QueueKey::worker_cache_volume_of
pub const CONTAINER_WORKER_CACHE_TABLE: &str = "container.worker_cache";
/// The key of [`CONTAINER_WORKER_CACHE_TABLE`].
const CONTAINER_WORKER_CACHE_DIRS: &str = "dirs";
/// `[container.verification]`: what the verification of `integrate` set to
/// the container is given beside its clone (ADR-t2257-1 decision 4 (b)).
/// Its one key, `prepare` (a non-blank string), is a shell command the
/// control side runs on the host before each such verification, in the
/// main checkout (never the run's code), with
/// [`MATERIALS_ENV`](crate::domain::run_container::MATERIALS_ENV) naming
/// an empty directory of the verification's that only the control side
/// writes: what it puts there (data the commands read, fetched where the
/// container cannot reach) the runtime copies into the run directory's
/// materials, following no symlink, and the verification's container
/// mounts that copy read-only, named by the same variable. A command that fails runs no
/// verification command. Without the table nothing runs, which is the
/// default. A binary older than this table refuses it, so a repository
/// adds it only once the binary that runs its queue reads it.
pub const CONTAINER_VERIFICATION_TABLE: &str = "container.verification";
/// The key of [`CONTAINER_VERIFICATION_TABLE`].
const CONTAINER_VERIFICATION_PREPARE: &str = "prepare";
/// `[toolchains.<name>]`: a toolchain profile the repository uses
/// (ADR-t2268-1 decision 3), one table per declaration, read in file
/// order. `<name>` is the declaration's own name, kebab-case (lowercase
/// letters and digits joined by `-`), bare or quoted, each once. Its keys:
///
/// - `profile` (a string, required): the profile, one of the runtime's
///   built-in profiles by name ([`crate::domain::toolchain::BUILTIN`]:
///   `rust`). A name the runtime does not build in is a mistake of the
///   file, which the reader of the declarations
///   ([`load_toolchains`]) and `doctor` report with its line.
/// - `paths` (a one-line array of distinct repository-relative globs,
///   optional): the paths the profile applies to; without it, the whole
///   repository. An empty array, a blank or absolute glob, or a `.` /
///   `..` / empty segment is a mistake of the file.
///
/// Without any table no profile is declared, and none reads the
/// verification's output: only the runtime's own marks and the project's
/// do ([`crate::domain::toolchain::Toolchains::none`], ADR-t2268-1
/// decision 3).
/// Every declared profile reads a verification command's output whatever
/// its paths (the commands run over the whole repository); what the paths
/// choose is on [`crate::domain::toolchain`]. Profiles are never inferred
/// from the files of the repository. A binary older than the table refuses
/// it, so a repository declares one only once the binary that runs its
/// queue reads it.
pub const TOOLCHAINS_PREFIX: &str = "toolchains.";
/// What [`parse_config`] calls the current table while in a
/// `[toolchains.*]`.
const TOOLCHAINS_TABLE: &str = "toolchains";
/// The keys of `[toolchains.<name>]` ([`TOOLCHAINS_PREFIX`]).
const TOOLCHAIN_PROFILE: &str = "profile";
const TOOLCHAIN_PATHS: &str = "paths";
/// `[verify_marks.<name>]`: a mark of the project's own on its
/// verification's output (ADR-t2268-1 decision 2), a line its own tools
/// or test helpers print that puts a failure down to one of the runtime's
/// classes and may name the test that failed; one table per mark, read in
/// file order. `<name>` is the mark's own name, kebab-case, bare or
/// quoted, each once. Its keys, both required:
///
/// - `class` (a string): the class the line gives, one the runtime has
///   and a mark may name: `killed` or `timeout` (the host's: integrate
///   tries the command once more instead of resuming the worker), or
///   `build_error`, `lint`, `test_failure`, `format` or `coverage_below`
///   (the code's). `disk_full`, `flaky` and `unknown` take no mark, and
///   any other name is a mistake of the file: a mark adds lines to a
///   class, never a class (ADR-t639-1 decision 5).
/// - `line` (a string): the shape of the whole line, without the spaces
///   around it: `*` stands for any text (none too), `{test}` (at most
///   once, never next to `*`) for the name of the
///   test that failed, and the rest is the line's text. A shape without
///   text of its own, or with `{test}` twice or beside `*`, is a mistake
///   of the file. The text in `{test}`'s place names a test when it is
///   letters, digits, `_` and `:`; else the line still gives the class
///   and names none.
///
/// For `tests/common`'s `within` in this repository:
///
/// ```toml
/// [verify_marks.within]
/// class = "timeout"
/// line = "test {test} timed out: *did not happen within*"
/// ```
///
/// The marks are read whatever profiles are declared, in the order
/// [`crate::domain::verify_failure::classify`] keeps for a profile's
/// marks: those of the host's classes after the runtime's full disk and
/// signal and the flaky tests, by their first line against a profile's
/// test out of time, and before the exit 124; those of the code's before
/// the profiles' code failures. A binary
/// older than the table refuses it, so a repository declares one only
/// once the binary that runs its queue reads it.
pub const VERIFY_MARKS_PREFIX: &str = "verify_marks.";
/// What [`parse_config`] calls the current table while in a
/// `[verify_marks.*]`.
const VERIFY_MARKS_TABLE: &str = "verify_marks";
/// The keys of `[verify_marks.<name>]` ([`VERIFY_MARKS_PREFIX`]).
const VERIFY_MARK_CLASS: &str = "class";
const VERIFY_MARK_LINE: &str = "line";
/// `[local_checks]`: `tests`, the globs of the paths that are tests
/// ([`LOCAL_GATES_PREFIX`]).
const LOCAL_CHECKS_TABLE: &str = "local_checks";
/// The one key of `[local_checks]`.
const LOCAL_CHECKS_TESTS: &str = "tests";
/// `[local_checks.gates.<name>]`: a check the worker runs before its
/// receipt when a test of the gate's paths changed (ADR-t2589-1 decision
/// 6), one table per gate, read in file order. `<name>` is the gate's own
/// name, kebab-case, bare or quoted, each once. Its keys, both required:
///
/// - `paths` (a one-line array of distinct repository-relative globs, not
///   empty): the tests that require the gate.
/// - `kind` (a string, not blank): the kind the project's scripts give
///   the records of a run of it ([`crate::domain::local_checks`]).
///
/// A gate needs `[local_checks] tests`, the globs of the paths that are
/// tests (an array as `paths`), and a changed test requires a gate when it
/// is in the gate's paths; which changed lines are a test's the declared
/// toolchain profiles tell ([`crate::domain::local_gates`]). For this
/// repository, for example:
///
/// ```toml
/// [local_checks]
/// tests = ["src/**", "tests/it/**"]
///
/// [local_checks.gates.stress]
/// paths = ["src/**", "tests/it/**"]
/// kind = "stress"
/// ```
///
/// The review reads them from the landing branch's commit and judges each
/// gate by the run's diff; without a gate it judges none and its material
/// has no section of them. A binary older than these tables refuses them,
/// so a repository declares them only once the binary that runs its queue
/// reads them.
pub const LOCAL_GATES_PREFIX: &str = "local_checks.gates.";
/// What [`parse_config`] calls the current table while in a
/// `[local_checks.gates.*]`.
const LOCAL_GATES_TABLE: &str = "local_checks.gates";
/// The keys of `[local_checks.gates.<name>]`.
const LOCAL_GATE_PATHS: &str = "paths";
const LOCAL_GATE_KIND: &str = "kind";
const CONTAINER_IMAGE_CONTAINERFILE: &str = "containerfile";
const CONTAINER_IMAGE_INPUTS: &str = "inputs";
/// `[landing.approval]` adds OR conditions to the existing human asks.
/// `paths` is an array using task scope globs; `review_confidence` is an
/// array accepting only "low"; `goal_marks` is an array of nonblank literal
/// names. All arrays default to empty. `two_person` is a boolean, default
/// false, requiring the owner plus a distinct eligible approver when open.
/// Unknown keys, invalid types or values and duplicate keys are errors.
/// Only the commit-pinned main reader can expose these as trusted conditions.
const LANDING_APPROVAL_TABLE: &str = "landing.approval";
const TABLES: [&str; 36] = [
    LANDING_APPROVAL_TABLE,
    RUN_ENV_TABLE,
    STALL_TABLE,
    CONFLICTS_TABLE,
    RECHECK_TABLE,
    DISK_TABLE,
    RESUME_TABLE,
    RETENTION_TABLE,
    EXIT_TABLE,
    REPOSITORY_TABLE,
    WORKER_TRIAL_TABLE,
    PROVIDER_TRIAL_TABLE,
    LANGUAGE_TABLE,
    SUPERVISOR_TABLE,
    AREAS_TABLE,
    TASKS_TABLE,
    GOALS_TABLE,
    E2E_TABLE,
    HEADLESS_TABLE,
    PROVIDER_FALLBACK_TABLE,
    FRESH_SESSION_TABLE,
    THROUGHPUT_REVIEW_TABLE,
    REPLAN_TABLE,
    REVIEW_JOBS_TABLE,
    REVIEW_TABLE,
    REVIEW_CODE_TABLE,
    CI_WATCH_TABLE,
    LANDING_VERIFICATION_TABLE,
    VERIFICATION_TABLE,
    EVAL_TABLE,
    EGRESS_TABLE,
    CONTAINER_IMAGE_TABLE,
    CONTAINER_BUILD_OUTPUT_TABLE,
    CONTAINER_WORKER_CACHE_TABLE,
    CONTAINER_VERIFICATION_TABLE,
    LOCAL_CHECKS_TABLE,
];
/// The keys of `[recheck]`.
const RECHECK_COMMAND: &str = "command";
const RECHECK_PATHS: &str = "paths";
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
    // Intentionally private: parse_config also reads untrusted run worktrees.
    landing_approval: crate::domain::landing_approval::ApprovalSettings,
    /// `[run.env]` as written, in file order, values unexpanded.
    pub run_env: Vec<(String, String)>,
    /// `[stall]`, the defaults for the keys it does not set. Each value is
    /// a positive integer of seconds, which may have `_` between digits and
    /// a `#` comment after it; an unknown key, a value that is not a
    /// positive integer or a key set twice is an error.
    pub stall: StallConfig,
    /// `[conflicts]`, the defaults for the keys it does not set: positive
    /// integers under the same rules as `[stall]`.
    pub conflicts: ConflictConfig,
    /// `[recheck]`: no command checks the merge only, no paths runs the
    /// command on every run.
    pub recheck: RecheckConfig,
    /// `[disk]`, the defaults for the keys it does not set.
    pub disk: DiskConfig,
    /// The retired keys of `[disk]` the file still sets
    /// ([`DiskConfig::RETIRED_KEYS`]), in file order: read on without
    /// their values, and warned of by [`load_disk_config`].
    pub disk_retired: Vec<String>,
    /// `[resume]`, the default for the key it does not set.
    pub resume: ResumeConfig,
    /// `[retention]` (ADR-t697-1), the defaults for the keys it does not
    /// set.
    pub retention: RetentionConfig,
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
    /// `runtime` of `[roles.worker]` and `[roles.verification]` (goal 38),
    /// the host where unset.
    pub runtimes: RuntimeConfig,
    /// `[supervisor]`, each key it sets (task 698).
    pub supervisor: SupervisorConfig,
    /// `[areas]` (ADR-t980-1); `None` without the table.
    pub areas: Option<AreaMap>,
    /// `[tasks] changes` (ADR-t980-1); `None` without it.
    pub changes: Option<ChangeSet>,
    /// `[tasks] title_prefixes`, the prefixes a task's title starts with,
    /// `<prefix>[(<scope>)][!]: <subject>`, which `lint` reports and
    /// `submit` refuses a task's title outside of; `None` without it.
    pub title_prefixes: Option<TitlePrefixes>,
    /// `[tasks] related_docs`, the globs (as `add --paths` takes them) of
    /// the documents `lint` looks in for its hints (ADR-t3321-1); `None`
    /// without it, and an empty list, like none, gives no hints.
    pub related_docs: Option<Vec<String>>,
    /// `[goals] tags` (ADR-t1639-1 decision 6); `None` without it.
    pub goal_tags: Option<TagSet>,
    /// `[goals] max_spike_cycles`, at least 1: the cycles (rounds of
    /// Spikes and the replanning of their results) a goal may have before a
    /// planner of the runtime's may submit no further Spike of it
    /// (ADR-t1487-1 decision 8 as ADR-t2414-1 amends it); `None` without
    /// it.
    pub max_spike_cycles: Option<usize>,
    /// `[e2e] paths` (ADR-t963-1 decision 2); empty without it.
    pub e2e_paths: Vec<String>,
    /// The declared command and resolved time limit; none without a command.
    pub e2e_command: Option<crate::application::install::E2eCommand>,
    /// `[review.subagents.<agent>]` in file order (ADR-t1453-1 decision
    /// 1); empty without any.
    pub review_subagents: Vec<ReviewSubagent>,
    /// `[review.subagents."builtin:<name>"]` of the built-in agents, in
    /// file order (ADR-t2259-1 decision 4); empty without any, which
    /// leaves each to its defaults. `builtin:acceptance` is checked and
    /// not kept.
    pub review_builtins: Vec<BuiltinSetting>,
    /// Optional coverage stage; never added to the parallel agent selection.
    pub acceptance_precheck: review_builtins::AcceptancePrecheck,
    /// `[review.programs.<name>]` in file order (ADR-t1895-2); empty
    /// without any.
    pub review_programs: Vec<ReviewProgram>,
    /// `[review.programs."builtin:<name>"]` of the built-in programs, in
    /// file order (ADR-t2259-1 decision 4); empty without any, which
    /// leaves each to its defaults.
    pub review_builtin_programs: Vec<BuiltinProgramSetting>,
    /// `[headless] wrapper` (ADR-t1404-1 decision 7); `None` without it,
    /// which is the default, a workspace.
    pub headless_wrapper: Option<HeadlessWrapper>,
    /// `[provider_fallback]` (ADR-t1857-1), the default (on) for the keys
    /// it does not set.
    pub provider_fallback: ProviderFallback,
    /// `[fresh_session]` (ADR-t2080-1); no key starts no new session.
    pub fresh_session: FreshSessionConfig,
    /// `[throughput_review]`, the default (`kpi_stats` on) for the keys it
    /// does not set.
    pub throughput_review: ThroughputReviewConfig,
    /// `[replan]`, each key's default where unset; without the table the
    /// runtime detects and diagnoses nothing.
    pub replan: ReplanConfig,
    /// `[review.jobs]` (ADR-t1895-1 decision 1); a kind without its key
    /// keeps the provider's review timeout, and no `concurrency` keeps
    /// its default.
    pub review_jobs: ReviewJobs,
    /// `[review.code] paths` ([`REVIEW_CODE_TABLE`]); `None` without the
    /// table or with no glob.
    pub review_code: Option<CodePaths>,
    /// `[review] revise_limit` ([`REVIEW_TABLE`]); `None` without it,
    /// which keeps the default limit.
    pub review_revise_limit: Option<ReviseLimit>,
    /// `[ci_watch]` (ADR-t1920-1); `None` without the table, which watches
    /// nothing.
    pub ci_watch: Option<CiWatchConfig>,
    /// `[landing_verification]` (ADR-t1925-1 decision 4); `None` without
    /// the table, which runs a task's commands as registered.
    pub landing_verification: Option<LandingVerification>,
    /// `[verification]`; no key leaves the runtime's limit of one
    /// verification command.
    pub verification: VerificationConfig,
    /// `[eval]` and `[eval.providers.<provider>]` (ADR-t1728-1), each key's
    /// default where unset.
    pub eval: EvalConfig,
    /// `[egress] registries` ([`EGRESS_REGISTRIES`]); empty without it.
    pub egress_registries: Vec<RegistryHost>,
    /// `[container.image]` (ADR-t2257-1 decision 2); `None` without the
    /// table, which runs the container runtime's base alone.
    pub container_image: Option<ProjectImage>,
    /// `[container.build_output] dir` ([`CONTAINER_BUILD_OUTPUT_TABLE`]);
    /// `None` without the table.
    pub container_build_output: Option<String>,
    /// `[container.worker_cache] dirs` ([`CONTAINER_WORKER_CACHE_TABLE`]);
    /// empty without the table.
    pub container_worker_cache: Vec<String>,
    /// `[container.verification] prepare`
    /// ([`CONTAINER_VERIFICATION_TABLE`]); `None` without the table.
    pub container_verification_prepare: Option<String>,
    /// `[toolchains.<name>]` in file order ([`TOOLCHAINS_PREFIX`]); empty
    /// without any. The profiles they name are looked up when they are
    /// resolved ([`load_toolchains`]).
    pub toolchains: Vec<ToolchainDeclaration>,
    /// `[verify_marks.<name>]` in file order ([`VERIFY_MARKS_PREFIX`]);
    /// empty without any.
    pub verify_marks: Vec<ProjectMark>,
    /// `[local_checks]` and its `[local_checks.gates.<name>]` in file
    /// order ([`LOCAL_GATES_PREFIX`]); `None` without a gate.
    pub local_gates: Option<LocalGates>,
    /// `[telemetry]` and `[[telemetry.commands]]`
    /// ([`crate::domain::command_rules::TelemetrySettings`]); the defaults
    /// and no rule without them.
    pub telemetry: crate::domain::command_rules::TelemetrySettings,
    /// `[[recurring]]` in file order (ADR-0072 decision 1); empty without
    /// any.
    pub recurring: Vec<crate::domain::recurring::RecurringDraft>,
}

impl Config {
    /// The review's agents as this `dagq.toml` writes them: the
    /// project's and the built-ins' tables.
    pub fn review_agents(&self) -> ReviewAgents {
        ReviewAgents {
            precheck: self.acceptance_precheck.clone(),
            agents: self.review_subagents.clone(),
            builtins: self.review_builtins.clone(),
            roles: self.roles.clone(),
        }
    }

    /// The program reviews as this `dagq.toml` writes them: the
    /// project's and the built-ins' tables.
    pub fn review_programs(&self) -> ReviewPrograms {
        ReviewPrograms {
            programs: self.review_programs.clone(),
            builtins: self.review_builtin_programs.clone(),
        }
    }
}

/// Parse the whole file.
///
/// A `[run.env]` value is a literal string (`'...'`, without escapes) or a
/// basic one (`"..."`, whose escapes are `\\`, `\"`, `\n` and `\t`; any
/// other is an error). A key before any table header, and a `[run.env]`
/// key that is not an environment variable name (a letter or `_`, then
/// letters, digits or `_`), are errors with their line number.
///
/// `[supervisor] light_changes` is checked against `[tasks] changes` only
/// after the whole file is read, so `[tasks]` may come after
/// `[supervisor]`; its error carries the line of `light_changes`.
/// Unknown tables or keys, duplicate keys and values of the wrong type or
/// range fail the whole file with a line number. The retired broker tables
/// are skipped regardless of their contents; language is checked separately.
/// A `run.env` key starting with `DAGQ_` is reserved for the runtime.
/// A `#` outside a string starts a comment.
/// A container runtime on a role other than worker or verification is refused
/// with `refuse_container`'s reason. Inbox has no role table: such a table is
/// an unknown role, rather than a way to select a runtime.
pub fn parse_config(text: &str) -> Result<Config> {
    let mut table: Option<&str> = None;
    let mut seen: Vec<&str> = Vec::new();
    let mut config = Config::default();
    let mut approval_keys: Vec<String> = Vec::new();
    let mut stall_keys: Vec<String> = Vec::new();
    let mut conflict_keys: Vec<String> = Vec::new();
    let mut disk_keys: Vec<String> = Vec::new();
    let mut disk_estimate_line = 0;
    // Defer reading the value until the method is known; recent ignores it.
    let mut disk_run_size: Option<(&str, usize)> = None;
    let mut disk_landing_size: Option<(&str, usize)> = None;
    let mut resume_keys: Vec<String> = Vec::new();
    let mut retention_keys: Vec<String> = Vec::new();
    let mut exit_keys: Vec<String> = Vec::new();
    let mut trial_keys: Vec<String> = Vec::new();
    let mut provider_trial_keys: Vec<String> = Vec::new();
    let mut supervisor_keys: Vec<String> = Vec::new();
    // `[supervisor] light_changes` and its line, checked against `[tasks]
    // changes` once the file is read whole (ADR-t1591-1).
    let mut light_changes: Option<(Vec<TaskChange>, usize)> = None;
    let mut fallback_keys: Vec<String> = Vec::new();
    let mut fresh_session_keys: Vec<String> = Vec::new();
    let mut verification_keys: Vec<String> = Vec::new();
    let mut throughput_review_keys: Vec<String> = Vec::new();
    let mut replan_keys: Vec<String> = Vec::new();
    let mut review_jobs_keys: Vec<String> = Vec::new();
    // `[ci_watch]`'s header line and the keys read, the table checked once
    // the file is read whole (its `workflow` is required).
    let mut ci_watch: Option<(usize, CiWatchConfig)> = None;
    let mut ci_watch_keys: Vec<String> = Vec::new();
    // `[landing_verification]`'s header line, replacement pair, and optional
    // mode; the pair is checked once the file is read whole.
    let mut landing: Option<LandingTable> = None;
    // `[container.image]`'s header line and its keys as read; its
    // `containerfile` is required, which is checked once the file is read
    // whole.
    let mut container_image: Option<(usize, ProjectImage)> = None;
    let mut container_image_keys: Vec<String> = Vec::new();
    // `[container.build_output]`'s header line; its `dir` is required.
    let mut build_output: Option<usize> = None;
    // `[container.worker_cache]`'s header line and whether its `dirs` was
    // read; `dirs` is required.
    let mut worker_cache: Option<(usize, bool)> = None;
    // `[container.verification]`'s header line; its `prepare` is required.
    let mut verification_prepare: Option<usize> = None;
    let mut role: Option<ModelRole> = None;
    let mut roles_seen: Vec<ModelRole> = Vec::new();
    // `[roles.worker]` or `[roles.verification]` being read in place of a
    // role of [`ModelRole`], and those read.
    let mut runtime_target: Option<RuntimeTarget> = None;
    let mut targets_seen: Vec<RuntimeTarget> = Vec::new();
    let mut role_keys: Vec<String> = Vec::new();
    let mut eval_keys: Vec<String> = Vec::new();
    // The provider of the `[eval.providers.<provider>]` being read and the
    // keys of the current one; those read are `declared_providers`.
    let mut eval_provider: Option<crate::domain::Provider> = None;
    let mut eval_provider_keys: Vec<String> = Vec::new();
    let mut kpi = KpiTables::default();
    let mut telemetry = TelemetryTables::default();
    let mut recurring = RecurringTables::default();
    let mut freshness = super::ci_freshness_config::FreshnessTables::default();
    let mut areas: Option<Vec<(String, Vec<String>)>> = None;
    let mut e2e_keys = Vec::new();
    let mut e2e_command = None;
    let mut e2e_timeout = None;
    let mut review_code_seen: Option<usize> = None;
    let mut egress_seen = false;
    let mut recheck_paths_seen = false;
    // The line of the `[review.subagents.<agent>]` header whose `paths`
    // is not read yet.
    let mut subagent_open: Option<usize> = None;
    // The keys of the current `[review.subagents.<agent>]` read.
    let mut subagent_keys: Vec<String> = Vec::new();
    // Whether `[review.subagents."builtin:acceptance"]` was read, and
    // whether it is the table being read (no setting of it is kept).
    let mut acceptance_seen = false;
    let mut in_acceptance = false;
    let mut precheck_seen = false;
    let mut in_precheck = false;
    // The `[review.programs.<name>]` being read, finished at the next
    // table or the end of the file.
    let mut program_open: Option<ProgramTable> = None;
    // The keys of the current `[review.programs."builtin:<name>"]` read.
    let mut builtin_program_keys: Vec<String> = Vec::new();
    // The keys of the current `[toolchains.<name>]` read.
    let mut toolchain_keys: Vec<String> = Vec::new();
    // The `[verify_marks.<name>]` being read, finished at the next table
    // or the end of the file.
    let mut mark_open: Option<MarkTable> = None;
    // `[local_checks] tests` as read, and the gates read, the last one
    // finished at the next table or the end of the file.
    let mut local_tests: Option<Vec<String>> = None;
    let mut gates: Vec<LocalGate> = Vec::new();
    let mut gate_open: Option<GateTable> = None;
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
            if let Some(open) = program_open.take() {
                config.review_programs.push(open.finish()?);
            }
            finish_toolchain(&config.toolchains)?;
            if let Some(open) = mark_open.take() {
                config.verify_marks.push(open.finish()?);
            }
            if let Some(open) = gate_open.take() {
                gates.push(open.finish()?);
            }
            if kpi
                .header(name)
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
            {
                table = Some(KPI_TABLE);
                continue;
            }
            if telemetry.header(name, number)? {
                table = Some(TELEMETRY_TABLE);
                continue;
            }
            if recurring.header(name, number)? {
                table = Some(RECURRING_TABLE);
                continue;
            }
            if freshness.header(name, number)? {
                table = Some(super::ci_freshness_config::CI_FRESHNESS_TABLE);
                continue;
            }
            if let Some(name) = name.strip_prefix(ROLES_PREFIX) {
                role_keys.clear();
                table = Some(ROLES_TABLE);
                if let Ok(target) = name.parse::<RuntimeTarget>() {
                    ensure!(
                        !targets_seen.contains(&target),
                        "{CONFIG_FILE_NAME}:{number}: [{ROLES_PREFIX}{name}] is defined twice"
                    );
                    targets_seen.push(target);
                    runtime_target = Some(target);
                    role = None;
                    continue;
                }
                runtime_target = None;
                let parsed: ModelRole = name.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{CONFIG_FILE_NAME}:{number}: unknown role [{ROLES_PREFIX}{name}]; the roles are {}",
                        ModelRole::ALL
                            .map(ModelRole::as_str)
                            .into_iter()
                            .chain(RuntimeTarget::ALL.map(RuntimeTarget::as_str))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
                ensure!(
                    !roles_seen.contains(&parsed),
                    "{CONFIG_FILE_NAME}:{number}: [{ROLES_PREFIX}{name}] is defined twice"
                );
                roles_seen.push(parsed);
                role = Some(parsed);
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
                subagent_keys.clear();
                in_precheck = agent == review_builtins::ACCEPTANCE_PRECHECK;
                if in_precheck {
                    ensure!(
                        !precheck_seen,
                        "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                    );
                    precheck_seen = true;
                    table = Some(REVIEW_BUILTIN_TABLE);
                    continue;
                }
                if agent == review_builtins::ACCEPTANCE {
                    ensure!(
                        !acceptance_seen,
                        "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                    );
                    acceptance_seen = true;
                    in_acceptance = true;
                    table = Some(REVIEW_BUILTIN_TABLE);
                    continue;
                }
                // A name of the built-ins' prefix is a built-in's, or a
                // mistake: no project's agent takes it.
                if let Some(builtin) = BuiltinAgent::named(&agent).map_err(|why| {
                    anyhow::anyhow!("{CONFIG_FILE_NAME}:{number}: [{name}]: {why}")
                })? {
                    ensure!(
                        config.review_builtins.iter().all(|b| b.agent != builtin),
                        "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                    );
                    config.review_builtins.push(BuiltinSetting {
                        agent: builtin,
                        enabled: None,
                        paths: None,
                        provider: None,
                        model: None,
                        effort: None,
                    });
                    in_acceptance = false;
                    table = Some(REVIEW_BUILTIN_TABLE);
                    continue;
                }
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
                    provider: None,
                    model: None,
                    effort: None,
                });
                subagent_open = Some(number);
                table = Some(REVIEW_SUBAGENTS_TABLE);
                continue;
            }
            if name == REVIEW_PROGRAMS_TABLE || name.starts_with(REVIEW_PROGRAMS_PREFIX) {
                let program = name
                    .strip_prefix(REVIEW_PROGRAMS_PREFIX)
                    .map(|program| parse_key(program.trim()))
                    .transpose()
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
                    .unwrap_or_default();
                ensure!(
                    !program.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] names no program; write [{REVIEW_PROGRAMS_PREFIX}<name>]"
                );
                // A name of the built-ins' prefix is a built-in's, or a
                // mistake: no project's program takes it.
                if let Some(builtin) = BuiltinProgram::named(&program).map_err(|why| {
                    anyhow::anyhow!("{CONFIG_FILE_NAME}:{number}: [{name}]: {why}")
                })? {
                    ensure!(
                        config
                            .review_builtin_programs
                            .iter()
                            .all(|b| b.program != builtin),
                        "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                    );
                    config.review_builtin_programs.push(BuiltinProgramSetting {
                        program: builtin,
                        enabled: None,
                        paths: None,
                    });
                    builtin_program_keys.clear();
                    table = Some(REVIEW_BUILTIN_PROGRAM_TABLE);
                    continue;
                }
                ensure!(
                    review_subagents::valid_agent_name(&program),
                    "{CONFIG_FILE_NAME}:{number}: program {program:?} of [{name}] is not kebab-case (lowercase letters and digits joined by -)"
                );
                ensure!(
                    config.review_programs.iter().all(|p| p.name != program),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                );
                program_open = Some(ProgramTable {
                    line: number,
                    name: program,
                    ..ProgramTable::default()
                });
                table = Some(REVIEW_PROGRAMS_TABLE);
                continue;
            }
            if name == TOOLCHAINS_TABLE || name.starts_with(TOOLCHAINS_PREFIX) {
                let toolchain = name
                    .strip_prefix(TOOLCHAINS_PREFIX)
                    .map(|toolchain| parse_key(toolchain.trim()))
                    .transpose()
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
                    .unwrap_or_default();
                ensure!(
                    !toolchain.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] names no toolchain; write [{TOOLCHAINS_PREFIX}<name>]"
                );
                ensure!(
                    review_subagents::valid_agent_name(&toolchain),
                    "{CONFIG_FILE_NAME}:{number}: toolchain {toolchain:?} of [{name}] is not kebab-case (lowercase letters and digits joined by -)"
                );
                ensure!(
                    config.toolchains.iter().all(|t| t.name != toolchain),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                );
                config.toolchains.push(ToolchainDeclaration {
                    name: toolchain,
                    profile: String::new(),
                    paths: Vec::new(),
                    line: number,
                });
                toolchain_keys.clear();
                table = Some(TOOLCHAINS_TABLE);
                continue;
            }
            if name == LOCAL_GATES_TABLE || name.starts_with(LOCAL_GATES_PREFIX) {
                let gate = name
                    .strip_prefix(LOCAL_GATES_PREFIX)
                    .map(|gate| parse_key(gate.trim()))
                    .transpose()
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
                    .unwrap_or_default();
                ensure!(
                    !gate.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] names no gate; write [{LOCAL_GATES_PREFIX}<name>]"
                );
                ensure!(
                    review_subagents::valid_agent_name(&gate),
                    "{CONFIG_FILE_NAME}:{number}: gate {gate:?} of [{name}] is not kebab-case (lowercase letters and digits joined by -)"
                );
                ensure!(
                    gates.iter().all(|g| g.name != gate),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                );
                gate_open = Some(GateTable {
                    line: number,
                    name: gate,
                    ..GateTable::default()
                });
                table = Some(LOCAL_GATES_TABLE);
                continue;
            }
            if name == VERIFY_MARKS_TABLE || name.starts_with(VERIFY_MARKS_PREFIX) {
                let mark = name
                    .strip_prefix(VERIFY_MARKS_PREFIX)
                    .map(|mark| parse_key(mark.trim()))
                    .transpose()
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}"))?
                    .unwrap_or_default();
                ensure!(
                    !mark.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] names no mark; write [{VERIFY_MARKS_PREFIX}<name>]"
                );
                ensure!(
                    review_subagents::valid_agent_name(&mark),
                    "{CONFIG_FILE_NAME}:{number}: mark {mark:?} of [{name}] is not kebab-case (lowercase letters and digits joined by -)"
                );
                ensure!(
                    config.verify_marks.iter().all(|m| m.name != mark),
                    "{CONFIG_FILE_NAME}:{number}: [{name}] is defined twice"
                );
                mark_open = Some(MarkTable {
                    line: number,
                    name: mark,
                    ..MarkTable::default()
                });
                table = Some(VERIFY_MARKS_TABLE);
                continue;
            }
            if let Some(name) = name.strip_prefix(EVAL_PROVIDERS_PREFIX) {
                let provider: crate::domain::Provider = name.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{CONFIG_FILE_NAME}:{number}: unknown provider [{EVAL_PROVIDERS_PREFIX}{name}]; the providers are claude, codex"
                    )
                })?;
                ensure!(
                    !config.eval.declared_providers.contains(&provider),
                    "{CONFIG_FILE_NAME}:{number}: [{EVAL_PROVIDERS_PREFIX}{name}] is defined twice"
                );
                config.eval.declared_providers.push(provider);
                eval_provider = Some(provider);
                eval_provider_keys.clear();
                table = Some(EVAL_PROVIDER_TABLE);
                continue;
            }
            if RETIRED_TABLES.contains(&name) {
                table = Some(RETIRED_TABLE);
                continue;
            }
            let known = TABLES.iter().find(|table| **table == name).with_context(|| {
                format!(
                    "{CONFIG_FILE_NAME}:{number}: unknown table [{name}]; only [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{RETENTION_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{PROVIDER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{REVIEW_SUBAGENTS_PREFIX}<agent>], [{REVIEW_PROGRAMS_PREFIX}<name>], [{TOOLCHAINS_PREFIX}<name>], [{VERIFY_MARKS_PREFIX}<name>], [{LOCAL_CHECKS_TABLE}], [{LOCAL_GATES_PREFIX}<name>], [{LANGUAGE_TABLE}], [{VERIFICATION_TABLE}], [{SUPERVISOR_TABLE}], [{AREAS_TABLE}], [{TASKS_TABLE}], [{GOALS_TABLE}], [{E2E_TABLE}], [{HEADLESS_TABLE}], [{PROVIDER_FALLBACK_TABLE}], [{THROUGHPUT_REVIEW_TABLE}], [{REVIEW_JOBS_TABLE}], [{REVIEW_TABLE}], [{REVIEW_CODE_TABLE}], [{CI_WATCH_TABLE}], [{LANDING_VERIFICATION_TABLE}], [{EVAL_TABLE}], [{EVAL_PROVIDERS_PREFIX}<provider>], [{EGRESS_TABLE}], [{CONTAINER_IMAGE_TABLE}], [{CONTAINER_BUILD_OUTPUT_TABLE}], [{CONTAINER_WORKER_CACHE_TABLE}], [{CONTAINER_VERIFICATION_TABLE}], [{TELEMETRY_TABLE}], [[{COMMANDS_TABLE}]], [[{RECURRING_TABLE}]] and [{KPI_TABLE}] are supported"
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
                        required_jobs: Vec::new(),
                        scheduled_workflows: Vec::new(),
                        freshness: Vec::new(),
                    },
                ));
            }
            if *known == AREAS_TABLE {
                areas.get_or_insert_with(Vec::new);
            }
            if *known == LANDING_VERIFICATION_TABLE {
                landing = Some(LandingTable {
                    line: number,
                    ..Default::default()
                });
            }
            if *known == CONTAINER_IMAGE_TABLE {
                container_image = Some((number, ProjectImage::default()));
            }
            if *known == CONTAINER_BUILD_OUTPUT_TABLE {
                build_output = Some(number);
            }
            if *known == CONTAINER_WORKER_CACHE_TABLE {
                worker_cache = Some((number, false));
            }
            if *known == CONTAINER_VERIFICATION_TABLE {
                verification_prepare = Some(number);
            }
            continue;
        }
        if table == Some(LANGUAGE_TABLE) || table == Some(RETIRED_TABLE) {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: expected KEY = value"))?;
        let key = key.trim();
        match table {
            Some(LANDING_APPROVAL_TABLE) => {
                ensure!(
                    !approval_keys.iter().any(|seen| seen == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                approval_keys.push(key.to_owned());
                let settings = &mut config.landing_approval;
                let with =
                    || format!("{CONFIG_FILE_NAME}:{number}: [{LANDING_APPROVAL_TABLE}] {key}");
                match key {
                    "paths" => {
                        let paths = parse_string_array(rest.trim()).with_context(with)?;
                        crate::domain::scope::validate_path_globs(&paths).with_context(with)?;
                        settings.paths = paths;
                    }
                    "review_confidence" => {
                        let values = parse_string_array(rest.trim()).with_context(with)?;
                        ensure!(
                            values.iter().all(|value| value == "low"),
                            "{CONFIG_FILE_NAME}:{number}: review_confidence accepts only low"
                        );
                        settings.low_confidence = !values.is_empty();
                    }
                    "goal_marks" => {
                        settings.goal_marks = parse_string_array(rest.trim())
                            .with_context(with)?
                            .into_iter()
                            .map(|name| {
                                crate::domain::landing_approval::GoalMark::new(name)
                                    .map_err(anyhow::Error::msg)
                            })
                            .collect::<Result<_>>()
                            .with_context(with)?;
                    }
                    "two_person" => {
                        settings.two_person = parse_bool(rest.trim()).with_context(with)?
                    }
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{LANDING_APPROVAL_TABLE}]"
                    ),
                }
            }
            Some(VERIFY_MARKS_TABLE) => mark_open
                .as_mut()
                .expect("a [verify_marks.*] table names its mark")
                .entry(number, key, rest.trim())?,
            Some(LOCAL_GATES_TABLE) => gate_open
                .as_mut()
                .expect("a [local_checks.gates.*] table names its gate")
                .entry(number, key, rest.trim())?,
            Some(LOCAL_CHECKS_TABLE) => {
                ensure!(
                    key == LOCAL_CHECKS_TESTS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{LOCAL_CHECKS_TABLE}]; the key is {LOCAL_CHECKS_TESTS}"
                );
                ensure!(
                    local_tests.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                local_tests = Some(nonempty_globs(number, key, rest)?);
            }
            Some(TOOLCHAINS_TABLE) => {
                let toolchain = config
                    .toolchains
                    .last_mut()
                    .expect("a [toolchains.*] table names its toolchain");
                ensure!(
                    key == TOOLCHAIN_PROFILE || key == TOOLCHAIN_PATHS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{TOOLCHAINS_PREFIX}{}]; the keys are {TOOLCHAIN_PROFILE} and {TOOLCHAIN_PATHS}",
                    toolchain.name
                );
                ensure!(
                    !toolchain_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                toolchain_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == TOOLCHAIN_PROFILE {
                    toolchain.profile = parse_string(rest.trim()).with_context(with)?;
                    ensure!(
                        !toolchain.profile.trim().is_empty(),
                        "{CONFIG_FILE_NAME}:{number}: {key} of [{TOOLCHAINS_PREFIX}{}] is blank",
                        toolchain.name
                    );
                } else {
                    let globs = parse_string_array(rest.trim()).with_context(with)?;
                    ensure!(
                        !globs.is_empty(),
                        "{CONFIG_FILE_NAME}:{number}: {key} of [{TOOLCHAINS_PREFIX}{}] names no glob; leave it out for the whole repository",
                        toolchain.name
                    );
                    validate_path_globs(&globs).with_context(with)?;
                    toolchain.paths = dedup_globs(&globs);
                }
            }
            Some(REVIEW_PROGRAMS_TABLE) => program_open
                .as_mut()
                .expect("a [review.programs.*] table names its program")
                .entry(number, key, rest.trim())?,
            Some(REVIEW_BUILTIN_PROGRAM_TABLE) => {
                let builtin = config
                    .review_builtin_programs
                    .last_mut()
                    .expect("a builtin table names its built-in");
                let section = format!("{REVIEW_PROGRAMS_PREFIX}\"{}\"", builtin.program.name());
                ensure!(
                    !builtin_program_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                builtin_program_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                match key {
                    REVIEW_SUBAGENT_ENABLED => {
                        builtin.enabled = Some(parse_bool(rest.trim()).with_context(with)?);
                    }
                    REVIEW_SUBAGENT_PATHS => {
                        let globs = parse_string_array(rest.trim()).with_context(with)?;
                        ensure!(
                            !globs.is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} of [{section}] names no glob; turn it off with {REVIEW_SUBAGENT_ENABLED} = false"
                        );
                        validate_path_globs(&globs).with_context(with)?;
                        builtin.paths = Some(dedup_globs(&globs));
                    }
                    "script" | "args" => bail!(
                        "{CONFIG_FILE_NAME}:{number}: [{section}] has {key}, which a built-in does not take: it runs the dagq binary's own script; turn it off with {REVIEW_SUBAGENT_ENABLED} = false and write a program of your own to run another"
                    ),
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{section}]; the keys are {REVIEW_SUBAGENT_ENABLED} and {REVIEW_SUBAGENT_PATHS}"
                    ),
                }
            }
            Some(REVIEW_BUILTIN_TABLE) => {
                if in_precheck {
                    ensure!(
                        !subagent_keys.iter().any(|existing| existing == key),
                        "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                    );
                    subagent_keys.push(key.to_owned());
                    let setting = &mut config.acceptance_precheck;
                    match key {
                        REVIEW_SUBAGENT_ENABLED => setting.enabled = parse_bool(rest.trim())?,
                        REVIEW_SUBAGENT_PROVIDER => {
                            setting.provider = Some(parse_string(rest.trim())?.parse()?)
                        }
                        REVIEW_SUBAGENT_MODEL => {
                            setting.model = Some(launch_value(key, rest, number)?)
                        }
                        REVIEW_SUBAGENT_EFFORT => {
                            setting.effort = Some(launch_value(key, rest, number)?)
                        }
                        _ => bail!(
                            "{CONFIG_FILE_NAME}:{number}: unknown key {key} in acceptance-precheck; only enabled/provider/model/effort are allowed"
                        ),
                    }
                    continue;
                }

                let section = if in_acceptance {
                    review_builtins::ACCEPTANCE.to_owned()
                } else {
                    config
                        .review_builtins
                        .last()
                        .expect("a builtin table names its built-in")
                        .agent
                        .name()
                };
                ensure!(
                    !subagent_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                subagent_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if in_acceptance {
                    // The whole review always runs, on every change, where
                    // `[roles.review]` sends it (ADR-t2259-1 decision 3).
                    ensure!(
                        key == REVIEW_SUBAGENT_ENABLED,
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_SUBAGENTS_PREFIX}\"{section}\"]; the whole review runs on every change on [roles.review]'s provider, so its only key is {REVIEW_SUBAGENT_ENABLED}"
                    );
                    ensure!(
                        parse_bool(rest.trim()).with_context(with)?,
                        "{CONFIG_FILE_NAME}:{number}: {section} cannot be turned off: the whole review is what a review's verdict stands on"
                    );
                    continue;
                }
                let builtin = config
                    .review_builtins
                    .last_mut()
                    .expect("a builtin table names its built-in");
                match key {
                    REVIEW_SUBAGENT_ENABLED => {
                        builtin.enabled = Some(parse_bool(rest.trim()).with_context(with)?);
                    }
                    REVIEW_SUBAGENT_PROVIDER => {
                        builtin.provider = Some(
                            parse_string(rest.trim())
                                .with_context(with)?
                                .parse::<crate::domain::Provider>()
                                .map_err(|error| anyhow::anyhow!("{error}"))
                                .with_context(with)?,
                        );
                    }
                    REVIEW_SUBAGENT_PATHS => {
                        let globs = parse_string_array(rest.trim()).with_context(with)?;
                        ensure!(
                            !globs.is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} of [{REVIEW_SUBAGENTS_PREFIX}\"{section}\"] names no glob; turn it off with {REVIEW_SUBAGENT_ENABLED} = false"
                        );
                        validate_path_globs(&globs).with_context(with)?;
                        builtin.paths = Some(dedup_globs(&globs));
                    }
                    REVIEW_SUBAGENT_MODEL => {
                        builtin.model = Some(launch_value(key, rest, number)?);
                    }
                    REVIEW_SUBAGENT_EFFORT => {
                        builtin.effort = Some(launch_value(key, rest, number)?);
                    }
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_SUBAGENTS_PREFIX}\"{section}\"]; the keys are {REVIEW_SUBAGENT_ENABLED}, {REVIEW_SUBAGENT_PATHS}, {REVIEW_SUBAGENT_PROVIDER}, {REVIEW_SUBAGENT_MODEL} and {REVIEW_SUBAGENT_EFFORT}"
                    ),
                }
            }
            Some(REVIEW_SUBAGENTS_TABLE) => {
                let agent = config
                    .review_subagents
                    .last_mut()
                    .expect("a [review.subagents.*] table names its agent");
                ensure!(
                    [
                        REVIEW_SUBAGENT_PATHS,
                        REVIEW_SUBAGENT_PROVIDER,
                        REVIEW_SUBAGENT_MODEL,
                        REVIEW_SUBAGENT_EFFORT
                    ]
                    .contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_SUBAGENTS_PREFIX}{}]; the keys are {REVIEW_SUBAGENT_PATHS}, {REVIEW_SUBAGENT_PROVIDER}, {REVIEW_SUBAGENT_MODEL} and {REVIEW_SUBAGENT_EFFORT}",
                    agent.name
                );
                ensure!(
                    !subagent_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                subagent_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == REVIEW_SUBAGENT_PROVIDER {
                    // A provider this binary does not know is a mistake of
                    // the configuration, so the review that reads it does
                    // not pass (ADR-t1453-1 decision 4).
                    let provider = parse_string(rest.trim())
                        .with_context(with)?
                        .parse::<crate::domain::Provider>()
                        .map_err(|error| anyhow::anyhow!("{error}"))
                        .with_context(with)?;
                    agent.provider = Some(provider);
                    continue;
                }
                if key == REVIEW_SUBAGENT_MODEL {
                    agent.model = Some(launch_value(key, rest, number)?);
                    continue;
                }
                if key == REVIEW_SUBAGENT_EFFORT {
                    agent.effort = Some(launch_value(key, rest, number)?);
                    continue;
                }
                subagent_open = None;
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
            Some(TASKS_TABLE) if key == TASKS_TITLE_PREFIXES => {
                ensure!(
                    config.title_prefixes.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let prefixes = parse_string_array(rest.trim()).with_context(with)?;
                config.title_prefixes = Some(
                    TitlePrefixes::new(prefixes)
                        .map_err(anyhow::Error::msg)
                        .with_context(with)?,
                );
            }
            Some(TASKS_TABLE) if key == TASKS_RELATED_DOCS => {
                ensure!(
                    config.related_docs.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let globs = parse_string_array(rest.trim()).with_context(with)?;
                validate_path_globs(&globs).with_context(with)?;
                config.related_docs = Some(dedup_globs(&globs));
            }
            Some(TASKS_TABLE) => {
                ensure!(
                    key == TASKS_CHANGES,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{TASKS_TABLE}]; the keys are {TASKS_CHANGES}, {TASKS_TITLE_PREFIXES} and {TASKS_RELATED_DOCS}"
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
            Some(GOALS_TABLE) if key == GOALS_SPIKE_CYCLES => {
                ensure!(
                    config.max_spike_cycles.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let limit = parse_positive(rest.trim(), "number").with_context(with)?;
                config.max_spike_cycles = Some(u16::try_from(limit).with_context(with)?.into());
            }
            Some(GOALS_TABLE) => {
                ensure!(
                    key == GOALS_TAGS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{GOALS_TABLE}]; the keys are {GOALS_TAGS}, {GOALS_SPIKE_CYCLES}"
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
            Some(EGRESS_TABLE) => {
                ensure!(
                    key == EGRESS_REGISTRIES,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{EGRESS_TABLE}]; the key is {EGRESS_REGISTRIES}"
                );
                ensure!(
                    !egress_seen,
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                egress_seen = true;
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let mut registries: Vec<RegistryHost> = Vec::new();
                for entry in parse_string_array(rest.trim()).with_context(with)? {
                    let registry: RegistryHost = entry
                        .parse()
                        .map_err(anyhow::Error::msg)
                        .with_context(with)?;
                    if !registries.contains(&registry) {
                        registries.push(registry);
                    }
                }
                config.egress_registries = registries;
            }
            Some(REVIEW_TABLE) => {
                ensure!(
                    key == REVIEW_REVISE_LIMIT,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_TABLE}]; the key is {REVIEW_REVISE_LIMIT}"
                );
                ensure!(
                    config.review_revise_limit.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let limit = parse_positive(rest.trim(), "number").with_context(with)?;
                let limit = usize::try_from(limit).with_context(with)?;
                config.review_revise_limit = Some(
                    ReviseLimit::new(limit)
                        .context("must be at least 1")
                        .with_context(with)?,
                );
            }
            Some(REVIEW_CODE_TABLE) => {
                ensure!(
                    key == REVIEW_CODE_PATHS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_CODE_TABLE}]; the key is {REVIEW_CODE_PATHS}"
                );
                ensure!(
                    review_code_seen.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                review_code_seen = Some(number);
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let globs = parse_string_array(rest.trim()).with_context(with)?;
                validate_path_globs(&globs).with_context(with)?;
                config.review_code = CodePaths::of(dedup_globs(&globs));
            }
            Some(E2E_TABLE) => {
                ensure!(
                    matches!(key, E2E_PATHS | "command" | "timeout_secs"),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{E2E_TABLE}]"
                );
                ensure!(
                    !e2e_keys.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                e2e_keys.push(key);
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                match key {
                    E2E_PATHS => {
                        let globs = parse_string_array(rest.trim()).with_context(with)?;
                        validate_path_globs(&globs).with_context(with)?;
                        config.e2e_paths = dedup_globs(&globs);
                    }
                    "command" => {
                        let command = parse_string(rest.trim()).with_context(with)?;
                        ensure!(
                            !command.trim().is_empty() && !command.contains(['\n', '\r']),
                            "{CONFIG_FILE_NAME}:{number}: command must be a non-empty single line"
                        );
                        e2e_command = Some(command);
                    }
                    _ => {
                        let secs =
                            parse_positive(rest.trim(), "number of seconds").with_context(with)?;
                        e2e_timeout = Some(Duration::from_secs(secs as u64));
                    }
                }
            }
            Some(CONTAINER_BUILD_OUTPUT_TABLE) => {
                ensure!(
                    key == CONTAINER_BUILD_OUTPUT_DIR,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONTAINER_BUILD_OUTPUT_TABLE}]; the key is {CONTAINER_BUILD_OUTPUT_DIR}"
                );
                ensure!(
                    config.container_build_output.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let dir = parse_string(rest.trim()).with_context(with)?;
                ensure!(
                    is_container_dir(&dir),
                    "{}: {dir:?} is not a directory of the container (absolute, not /, without , : or ..)",
                    with()
                );
                config.container_build_output = Some(dir);
            }
            Some(CONTAINER_WORKER_CACHE_TABLE) => {
                ensure!(
                    key == CONTAINER_WORKER_CACHE_DIRS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONTAINER_WORKER_CACHE_TABLE}]; the key is {CONTAINER_WORKER_CACHE_DIRS}"
                );
                let (_, read) = worker_cache
                    .as_mut()
                    .expect("[container.worker_cache] was opened");
                ensure!(
                    !*read,
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                *read = true;
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let dirs = parse_string_array(rest.trim()).with_context(with)?;
                for (index, dir) in dirs.iter().enumerate() {
                    ensure!(
                        is_container_dir(dir),
                        "{}: {dir:?} is not a directory of the container (absolute, not /, without , : or ..)",
                        with()
                    );
                    let same =
                        |other: &String| other.trim_end_matches('/') == dir.trim_end_matches('/');
                    ensure!(
                        !dirs[..index].iter().any(same),
                        "{}: {dir:?} is listed twice",
                        with()
                    );
                }
                config.container_worker_cache = dirs;
            }
            Some(CONTAINER_VERIFICATION_TABLE) => {
                ensure!(
                    key == CONTAINER_VERIFICATION_PREPARE,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONTAINER_VERIFICATION_TABLE}]; the key is {CONTAINER_VERIFICATION_PREPARE}"
                );
                ensure!(
                    config.container_verification_prepare.is_none(),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let command = parse_string(rest.trim()).with_context(with)?;
                ensure!(!command.trim().is_empty(), "{}: it is blank", with());
                config.container_verification_prepare = Some(command);
            }
            Some(CONTAINER_IMAGE_TABLE) => {
                ensure!(
                    key == CONTAINER_IMAGE_CONTAINERFILE || key == CONTAINER_IMAGE_INPUTS,
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{CONTAINER_IMAGE_TABLE}]; the keys are {CONTAINER_IMAGE_CONTAINERFILE} and {CONTAINER_IMAGE_INPUTS}"
                );
                ensure!(
                    !container_image_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                container_image_keys.push(key.to_owned());
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let image = &mut container_image
                    .as_mut()
                    .expect("[container.image] was opened")
                    .1;
                if key == CONTAINER_IMAGE_CONTAINERFILE {
                    let path = parse_string(rest.trim()).with_context(with)?;
                    if let Some(problem) = repository_path_problem(&path) {
                        bail!("{}: {problem}", with());
                    }
                    image.containerfile = path;
                } else {
                    let paths = parse_string_array(rest.trim()).with_context(with)?;
                    for path in &paths {
                        if let Some(problem) = repository_path_problem(path) {
                            bail!("{}: {problem}", with());
                        }
                    }
                    image.inputs = paths;
                }
            }
            Some(TELEMETRY_TABLE) => telemetry.entry(number, key, rest.trim())?,
            Some(RECURRING_TABLE) => recurring.entry(number, key, rest.trim())?,
            Some(super::ci_freshness_config::CI_FRESHNESS_TABLE) => {
                freshness.entry(number, key, rest.trim())?;
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
            Some(FRESH_SESSION_TABLE) => {
                ensure!(
                    FreshSessionConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{FRESH_SESSION_TABLE}]; the keys are {}",
                    FreshSessionConfig::KEYS.join(", ")
                );
                ensure!(
                    !fresh_session_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let tokens = parse_positive(rest.trim(), "number of tokens").with_context(with)?;
                config.fresh_session.peak_context_above =
                    Some(u64::try_from(tokens).with_context(with)?);
                fresh_session_keys.push(key.to_owned());
            }
            Some(VERIFICATION_TABLE) => {
                ensure!(
                    verification_limit::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{VERIFICATION_TABLE}]; the keys are {}",
                    verification_limit::KEYS.join(", ")
                );
                ensure!(
                    !verification_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                if key == verification_limit::LANDING_BATCH_SIZE {
                    let size = parse_positive(rest.trim(), "batch size").with_context(with)?;
                    config.verification.landing_batch_size =
                        std::num::NonZeroUsize::new(usize::try_from(size)?)
                            .context("the landing batch size must be positive")?;
                } else {
                    let secs =
                        parse_positive(rest.trim(), "number of seconds").with_context(with)?;
                    config.verification.command_timeout_secs = Some(
                        verification_limit::checked_timeout_secs(secs)
                            .map_err(anyhow::Error::msg)
                            .with_context(with)?,
                    );
                }
                verification_keys.push(key.to_owned());
            }
            Some(THROUGHPUT_REVIEW_TABLE) => {
                ensure!(
                    ThroughputReviewConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{THROUGHPUT_REVIEW_TABLE}]; the keys are {}",
                    ThroughputReviewConfig::KEYS.join(", ")
                );
                ensure!(
                    !throughput_review_keys
                        .iter()
                        .any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                config.throughput_review.kpi_stats = parse_bool(rest.trim())
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                throughput_review_keys.push(key.to_owned());
            }
            Some(REPLAN_TABLE) => {
                ensure!(
                    ReplanConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REPLAN_TABLE}]; the keys are {}",
                    ReplanConfig::KEYS.join(", ")
                );
                ensure!(
                    !replan_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let count = |what| -> Result<usize> {
                    usize::try_from(parse_positive(rest.trim(), what).with_context(with)?)
                        .with_context(with)
                };
                let replan = &mut config.replan;
                match key {
                    "enabled" => replan.enabled = parse_bool(rest.trim()).with_context(with)?,
                    "non_pass_reviews" => replan.non_pass_reviews = count("number of reviews")?,
                    "continue_reviews" => replan.continue_reviews = count("number of reviews")?,
                    _ => replan.max_auto_parks = count("number of parks")?,
                }
                replan_keys.push(key.to_owned());
            }
            Some(EVAL_TABLE) => {
                ensure!(
                    EvalConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{EVAL_TABLE}]; the keys are {}",
                    EvalConfig::KEYS.join(", ")
                );
                ensure!(
                    !eval_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let eval = &mut config.eval;
                match key {
                    "max_runs" => {
                        eval.max_runs = u32::try_from(
                            parse_positive(rest.trim(), "number of runs").with_context(with)?,
                        )
                        .with_context(with)?;
                    }
                    "concurrency" => {
                        eval.concurrency = usize::try_from(
                            parse_positive(rest.trim(), "number of processes")
                                .with_context(with)?,
                        )
                        .with_context(with)?;
                    }
                    "recent_runs" => {
                        eval.recent_runs = usize::try_from(
                            parse_positive(rest.trim(), "number of runs").with_context(with)?,
                        )
                        .with_context(with)?;
                    }
                    "watch" => {
                        eval.watch.enabled = parse_bool(rest.trim()).with_context(with)?;
                    }
                    "watch_new_reviews" => {
                        eval.watch.new_reviews = usize::try_from(
                            parse_positive(rest.trim(), "number of reviews").with_context(with)?,
                        )
                        .with_context(with)?;
                    }
                    "watch_interval_days" => {
                        eval.watch.interval_days = u32::try_from(
                            parse_positive(rest.trim(), "number of days").with_context(with)?,
                        )
                        .with_context(with)?;
                    }
                    "max_cost_usd" => {
                        eval.max_cost_usd =
                            parse_positive_number(rest.trim()).with_context(with)?;
                    }
                    _ => {
                        let threshold = parse_positive_number(rest.trim()).with_context(with)?;
                        ensure!(
                            threshold <= 1.0,
                            "{CONFIG_FILE_NAME}:{number}: {key} must be at most 1, not {threshold}"
                        );
                        eval.threshold = threshold;
                    }
                }
                eval_keys.push(key.to_owned());
            }
            Some(EVAL_PROVIDER_TABLE) => {
                let provider = eval_provider.expect("[eval.providers.*] names its provider");
                ensure!(
                    ProviderCost::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{EVAL_PROVIDERS_PREFIX}{}]; the keys are {}",
                    provider.as_str(),
                    ProviderCost::KEYS.join(", ")
                );
                ensure!(
                    !eval_provider_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let dollars = parse_positive_number(rest.trim()).with_context(with)?;
                config.eval.provider_mut(provider).set(key, dollars);
                eval_provider_keys.push(key.to_owned());
            }
            Some(REVIEW_JOBS_TABLE) => {
                ensure!(
                    ReviewJobs::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_JOBS_TABLE}]; the keys are {}",
                    ReviewJobs::KEYS.join(", ")
                );
                ensure!(
                    !review_jobs_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let what = if key == "concurrency" {
                    "number of jobs"
                } else {
                    "number of seconds"
                };
                let value = parse_positive(rest.trim(), what).with_context(with)?;
                config
                    .review_jobs
                    .set(key, u64::try_from(value).with_context(with)?);
                review_jobs_keys.push(key.to_owned());
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
                    "required_jobs" | "scheduled_workflows" => {
                        let names = parse_string_array(rest.trim()).with_context(with)?;
                        let what = if key == "required_jobs" {
                            "job name"
                        } else {
                            "workflow"
                        };
                        for (index, name) in names.iter().enumerate() {
                            ensure!(
                                !name.trim().is_empty(),
                                "{CONFIG_FILE_NAME}:{number}: {key} has a blank {what}"
                            );
                            ensure!(
                                !names[..index].contains(name),
                                "{CONFIG_FILE_NAME}:{number}: {key} names {name:?} twice"
                            );
                        }
                        if key == "required_jobs" {
                            config.required_jobs = names;
                        } else {
                            config.scheduled_workflows = names;
                        }
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
            Some(LANDING_VERIFICATION_TABLE) => {
                let LandingTable {
                    replaces,
                    command,
                    non_incremental,
                    ..
                } = landing.as_mut().expect("[landing_verification] was opened");
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                match key {
                    landing_verification::NON_INCREMENTAL => {
                        ensure!(
                            non_incremental.is_none(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                        );
                        *non_incremental = Some(parse_bool(rest.trim()).with_context(with)?);
                    }
                    landing_verification::REPLACES => {
                        ensure!(
                            replaces.is_none(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                        );
                        let texts = parse_string_array(rest.trim()).with_context(with)?;
                        ensure!(
                            !texts.is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is empty; leave the table out to run the task's commands as registered"
                        );
                        for (index, text) in texts.iter().enumerate() {
                            ensure!(
                                !text.trim().is_empty(),
                                "{CONFIG_FILE_NAME}:{number}: {key} has a blank text"
                            );
                            ensure!(
                                !texts[..index].contains(text),
                                "{CONFIG_FILE_NAME}:{number}: {key} names {text:?} twice"
                            );
                        }
                        *replaces = Some(texts);
                    }
                    landing_verification::COMMAND => {
                        ensure!(
                            command.is_none(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                        );
                        let text = parse_string(rest.trim()).with_context(with)?;
                        ensure!(
                            !text.trim().is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                        );
                        *command = Some(text);
                    }
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{LANDING_VERIFICATION_TABLE}]; the keys are {}",
                        landing_verification::KEYS.join(", ")
                    ),
                }
            }
            Some(RECHECK_TABLE) => {
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                match key {
                    RECHECK_COMMAND => {
                        ensure!(
                            config.recheck.command.is_none(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                        );
                        let command = parse_string(rest.trim()).with_context(with)?;
                        ensure!(
                            !command.trim().is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is blank"
                        );
                        config.recheck.command = Some(command);
                    }
                    RECHECK_PATHS => {
                        ensure!(
                            !recheck_paths_seen,
                            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                        );
                        recheck_paths_seen = true;
                        let globs = parse_string_array(rest.trim()).with_context(with)?;
                        ensure!(
                            !globs.is_empty(),
                            "{CONFIG_FILE_NAME}:{number}: {key} is empty; leave it out to run the command on every run"
                        );
                        validate_path_globs(&globs).with_context(with)?;
                        config.recheck.paths = dedup_globs(&globs);
                    }
                    _ => bail!(
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{RECHECK_TABLE}]; the keys are {RECHECK_COMMAND} and {RECHECK_PATHS}"
                    ),
                }
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
                ensure!(
                    !role_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                role_keys.push(key.to_owned());
                if let Some(target) = runtime_target {
                    ensure!(
                        key == RUNTIME_KEY,
                        "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{ROLES_PREFIX}{}]; its only key is {RUNTIME_KEY}",
                        target.as_str()
                    );
                    let runtime = parse_runtime(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.runtimes.set(target, runtime);
                    continue;
                }
                let role = role.expect("a [roles.*] table names its role");
                if key == RUNTIME_KEY {
                    // Only the worker and the verification run untrusted
                    // code; the jobs and the planners stay on the host
                    // (ADR-t2113-3 decisions 2 and 3).
                    let runtime = parse_runtime(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    ensure!(
                        runtime == Runtime::Host,
                        "{CONFIG_FILE_NAME}:{number}: {}",
                        refuse_container(&format!("[{ROLES_PREFIX}{}]", role.as_str()))
                    );
                    continue;
                }
                ensure!(
                    RoleModel::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{ROLES_PREFIX}{}]; the keys are {}, {RUNTIME_KEY}",
                    role.as_str(),
                    RoleModel::KEYS.join(", ")
                );
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
                let trial = &mut config.worker_trial;
                match key {
                    "enabled" => trial.enabled = parse_bool(value).with_context(with)?,
                    "limited" => trial.limited = parse_bool(value).with_context(with)?,
                    "window" => {
                        let window = parse_positive(value, "number").with_context(with)?;
                        trial.window = usize::try_from(window).with_context(with)?;
                    }
                    "treatment_percent" => {
                        let digits = strip_comment(value);
                        let percent = digits
                            .parse::<u8>()
                            .ok()
                            .filter(|percent| *percent <= 100)
                            .with_context(|| {
                                format!("expected a whole percent from 0 to 100, not {digits}")
                            })
                            .with_context(with)?;
                        trial.treatment_percent = percent;
                    }
                    _ => {
                        let text = parse_string(value).with_context(with)?;
                        let field = match key {
                            "name" => &mut trial.name,
                            "control_model" => &mut trial.control.model,
                            "control_effort" => &mut trial.control.effort,
                            "treatment_model" => &mut trial.treatment.model,
                            _ => &mut trial.treatment.effort,
                        };
                        *field = text;
                    }
                }
                trial_keys.push(key.to_owned());
            }
            Some(PROVIDER_TRIAL_TABLE) => {
                ensure!(
                    ProviderTrial::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{PROVIDER_TRIAL_TABLE}]; the keys are {}",
                    ProviderTrial::KEYS.join(", ")
                );
                ensure!(
                    !provider_trial_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let value = rest.trim();
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let trial = &mut config.worker_trial.providers;
                match key {
                    "enabled" => trial.enabled = parse_bool(value).with_context(with)?,
                    "treatment_percent" => {
                        let digits = strip_comment(value);
                        trial.treatment_percent = digits
                            .parse::<u8>()
                            .ok()
                            .filter(|percent| *percent <= 100)
                            .with_context(|| {
                                format!("expected a whole percent from 0 to 100, not {digits}")
                            })
                            .with_context(with)?;
                    }
                    "name" => trial.name = parse_string(value).with_context(with)?,
                    _ => {
                        let provider = parse_string(value)
                            .with_context(with)?
                            .parse::<crate::domain::Provider>()
                            .map_err(|error| anyhow::anyhow!("{error}"))
                            .with_context(with)?;
                        if key == "treatment_worker" {
                            trial.treatment_worker = provider;
                        } else {
                            trial.treatment_review = provider;
                        }
                    }
                }
                provider_trial_keys.push(key.to_owned());
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
                } else if key == "max_spikes" {
                    let limit = parse_positive(rest.trim(), "number").with_context(with)?;
                    config.supervisor.max_spikes =
                        Some(u16::try_from(limit).with_context(with)?.into());
                } else if key == "max_investigations" {
                    let limit = parse_positive(rest.trim(), "number").with_context(with)?;
                    config.supervisor.max_investigations =
                        Some(u16::try_from(limit).with_context(with)?.into());
                } else if key == "class_aging" {
                    let secs = parse_positive(rest.trim(), "number").with_context(with)?;
                    let secs = u32::try_from(secs).with_context(with)?;
                    config.supervisor.class_aging = Some(usize::try_from(secs)?);
                } else if key == "claim_spacing" {
                    let secs = parse_whole(rest.trim()).with_context(with)?;
                    let secs = u32::try_from(secs).with_context(with)?;
                    config.supervisor.claim_spacing = Some(usize::try_from(secs)?);
                } else if key == "admission_stall_passes" {
                    let passes = parse_whole(rest.trim()).with_context(with)?;
                    config.supervisor.admission_stall_passes =
                        Some(u32::try_from(passes).with_context(with)?);
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
                if DiskConfig::RETIRED_KEYS.contains(&key) {
                    if !config.disk_retired.iter().any(|seen| seen == key) {
                        config.disk_retired.push(key.to_owned());
                    }
                    continue;
                }
                ensure!(
                    DiskConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{DISK_TABLE}]; the keys are {}",
                    DiskConfig::KEYS.join(", ")
                );
                ensure!(
                    !disk_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                if key == "reserve" {
                    let text = parse_string(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.disk.reserve = crate::domain::disk::parse_reserve(&text)
                        .with_context(|| {
                            format!(
                                "{CONFIG_FILE_NAME}:{number}: value of {key}: expected a share of the volume such as \"5%\" (above 0% and below 100%), not {text:?}"
                            )
                        })?;
                } else if key == "estimate" {
                    disk_estimate_line = number;
                    let text = parse_string(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.disk.estimate = match text.as_str() {
                        "recent" => DiskEstimate::Recent,
                        "fixed" => DiskEstimate::Fixed,
                        _ => anyhow::bail!(
                            "{CONFIG_FILE_NAME}:{number}: value of {key}: expected \"recent\" or \"fixed\", not {text:?}"
                        ),
                    };
                } else if key == "waiting_build_cleanup" {
                    let text = parse_string(rest.trim())
                        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
                    config.disk.waiting_build_cleanup = match text.as_str() {
                        "eager" => WaitingBuildCleanup::Eager,
                        "lazy" => WaitingBuildCleanup::Lazy,
                        "never" => WaitingBuildCleanup::Never,
                        _ => anyhow::bail!(
                            "{CONFIG_FILE_NAME}:{number}: value of {key}: expected \"eager\", \"lazy\" or \"never\", not {text:?}"
                        ),
                    };
                } else if key == "landing_size" {
                    disk_landing_size = Some((rest.trim(), number));
                } else {
                    disk_run_size = Some((rest.trim(), number));
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
            Some(RETENTION_TABLE) => {
                ensure!(
                    RetentionConfig::KEYS.contains(&key),
                    "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{RETENTION_TABLE}]; the keys are {}",
                    RetentionConfig::KEYS.join(", ")
                );
                ensure!(
                    !retention_keys.iter().any(|existing| existing == key),
                    "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
                );
                let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
                let value = parse_positive(rest.trim(), "number").with_context(with)?;
                if key == "days" {
                    config.retention.days = u64::try_from(value).with_context(with)?;
                } else {
                    config.retention.backups = usize::try_from(value).with_context(with)?;
                }
                retention_keys.push(key.to_owned());
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
                "{CONFIG_FILE_NAME}:{number}: a key outside [{RUN_ENV_TABLE}], [{STALL_TABLE}], [{CONFLICTS_TABLE}], [{RECHECK_TABLE}], [{DISK_TABLE}], [{RESUME_TABLE}], [{EXIT_TABLE}], [{REPOSITORY_TABLE}], [{WORKER_TRIAL_TABLE}], [{PROVIDER_TRIAL_TABLE}], [{ROLES_PREFIX}<role>], [{TOOLCHAINS_PREFIX}<name>], [{VERIFY_MARKS_PREFIX}<name>], [{LOCAL_CHECKS_TABLE}], [{LOCAL_GATES_PREFIX}<name>], [{SUPERVISOR_TABLE}], [{AREAS_TABLE}], [{TASKS_TABLE}], [{GOALS_TABLE}], [{HEADLESS_TABLE}], [{PROVIDER_FALLBACK_TABLE}], [{THROUGHPUT_REVIEW_TABLE}], [{REVIEW_JOBS_TABLE}], [{REVIEW_TABLE}], [{REVIEW_CODE_TABLE}], [{CI_WATCH_TABLE}], [{LANDING_VERIFICATION_TABLE}], [{CONTAINER_IMAGE_TABLE}], [{TELEMETRY_TABLE}], [[{COMMANDS_TABLE}]], [[{RECURRING_TABLE}]] or [{KPI_TABLE}]"
            ),
        }
    }
    if let Some(open) = program_open {
        config.review_programs.push(open.finish()?);
    }
    finish_toolchain(&config.toolchains)?;
    if let Some(open) = mark_open {
        config.verify_marks.push(open.finish()?);
    }
    if let Some(open) = gate_open {
        gates.push(open.finish()?);
    }
    if !gates.is_empty() {
        config.local_gates = Some(LocalGates {
            tests: local_tests.with_context(|| {
                format!(
                    "{CONFIG_FILE_NAME}: [{LOCAL_GATES_PREFIX}{}] needs [{LOCAL_CHECKS_TABLE}] {LOCAL_CHECKS_TESTS}, the globs of the tests",
                    gates[0].name
                )
            })?,
            gates,
        });
    }
    if let Some(line) = subagent_open {
        bail!(
            "{CONFIG_FILE_NAME}:{line}: [{REVIEW_SUBAGENTS_PREFIX}{}] has no {REVIEW_SUBAGENT_PATHS}",
            config.review_subagents.last().expect("an open agent").name
        );
    }
    // `[[ci_watch.freshness]]` belongs to `[ci_watch]` (ADR-t3924-1).
    let (freshness_line, freshness) = freshness.finish()?;
    if let Some((line, mut watch)) = ci_watch {
        ensure!(
            !watch.workflow.is_empty(),
            "{CONFIG_FILE_NAME}:{line}: [{CI_WATCH_TABLE}] has no workflow"
        );
        watch.freshness = freshness;
        config.ci_watch = Some(watch);
    } else if let Some(line) = freshness_line {
        bail!(
            "{CONFIG_FILE_NAME}:{line}: [[{}]] needs [{CI_WATCH_TABLE}]",
            super::ci_freshness_config::CI_FRESHNESS_TABLE
        );
    }
    if let Some(line) = build_output {
        ensure!(
            config.container_build_output.is_some(),
            "{CONFIG_FILE_NAME}:{line}: [{CONTAINER_BUILD_OUTPUT_TABLE}] has no {CONTAINER_BUILD_OUTPUT_DIR}"
        );
    }
    if let Some((line, read)) = worker_cache {
        ensure!(
            read,
            "{CONFIG_FILE_NAME}:{line}: [{CONTAINER_WORKER_CACHE_TABLE}] has no {CONTAINER_WORKER_CACHE_DIRS}"
        );
        if let Some(output) = &config.container_build_output {
            ensure!(
                !config
                    .container_worker_cache
                    .iter()
                    .any(|dir| dir.trim_end_matches('/') == output.trim_end_matches('/')),
                "{CONFIG_FILE_NAME}:{line}: [{CONTAINER_WORKER_CACHE_TABLE}] lists {output:?}, the directory of [{CONTAINER_BUILD_OUTPUT_TABLE}]"
            );
        }
    }
    if let Some(line) = verification_prepare {
        ensure!(
            config.container_verification_prepare.is_some(),
            "{CONFIG_FILE_NAME}:{line}: [{CONTAINER_VERIFICATION_TABLE}] has no {CONTAINER_VERIFICATION_PREPARE}"
        );
    }
    if let Some((line, image)) = container_image {
        ensure!(
            !image.containerfile.is_empty(),
            "{CONFIG_FILE_NAME}:{line}: [{CONTAINER_IMAGE_TABLE}] has no {CONTAINER_IMAGE_CONTAINERFILE}"
        );
        config.container_image = Some(image);
    }
    if let Some(LandingTable {
        line,
        replaces,
        command,
        non_incremental,
    }) = landing
    {
        let missing = |key: &str| {
            format!("{CONFIG_FILE_NAME}:{line}: [{LANDING_VERIFICATION_TABLE}] has no {key}")
        };
        let only_mode = replaces.is_none() && command.is_none() && non_incremental.is_some();
        config.landing_verification = Some(LandingVerification {
            non_incremental: non_incremental.unwrap_or(false),
            replaces: if only_mode {
                Vec::new()
            } else {
                replaces.with_context(|| missing(landing_verification::REPLACES))?
            },
            command: if only_mode {
                String::new()
            } else {
                command.with_context(|| missing(landing_verification::COMMAND))?
            },
        });
    }
    config.kpi = kpi.finish().with_context(|| CONFIG_FILE_NAME.to_owned())?;
    config.telemetry = telemetry.finish()?;
    config.recurring = recurring.finish()?;
    // A provider is checked against its role once the table is read whole
    // (ADR-t1063-1 decision 1).
    config
        .roles
        .check()
        .map_err(anyhow::Error::msg)
        .with_context(|| CONFIG_FILE_NAME.to_owned())?;
    config.areas = areas
        .map(|areas| AreaMap::new(areas).map_err(anyhow::Error::msg))
        .transpose()
        .with_context(|| format!("{CONFIG_FILE_NAME}: [{AREAS_TABLE}]"))?;
    // A candidate off the ladder is refused whole, its keys read
    // (ADR-t3957-1 decision 2).
    config
        .worker_trial
        .check()
        .map_err(|error| anyhow::anyhow!("{CONFIG_FILE_NAME}: [{WORKER_TRIAL_TABLE}]: {error}"))?;
    // So is a provider trial whose treatment's worker and review are one
    // provider, or one that would draw as the model trial does
    // (ADR-t4005-1 decision 2).
    let providers = &config.worker_trial.providers;
    providers
        .check()
        .and_then(|()| {
            // Only a trial turned on is compared: a `[worker.trial]` whose
            // name is the provider trial's default stays as it was.
            if providers.enabled && providers.name == config.worker_trial.name {
                Err(format!(
                    "name {:?} is the name of [{WORKER_TRIAL_TABLE}]; the two trials must draw independently",
                    providers.name
                ))
            } else {
                Ok(())
            }
        })
        .map_err(|error| anyhow::anyhow!("{CONFIG_FILE_NAME}: [{PROVIDER_TRIAL_TABLE}]: {error}"))?;
    if let Some((changes, line)) = light_changes {
        config.supervisor.light_changes = Some(
            LightChanges::new(changes, config.changes.as_ref())
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("{CONFIG_FILE_NAME}:{line}: value of light_changes"))?,
        );
    }
    ensure!(
        e2e_timeout.is_none() || e2e_command.is_some(),
        "{CONFIG_FILE_NAME}: [e2e] timeout_secs requires command"
    );
    config.e2e_command = e2e_command.map(|command| crate::application::install::E2eCommand {
        command,
        timeout: e2e_timeout.unwrap_or(crate::application::install::E2E_TIMEOUT),
    });
    for (key, value) in [
        ("run_size", disk_run_size),
        ("landing_size", disk_landing_size),
    ] {
        match config.disk.estimate {
            DiskEstimate::Fixed => {
                let Some((value, line)) = value else {
                    ensure!(
                        key != "run_size",
                        "{CONFIG_FILE_NAME}:{disk_estimate_line}: estimate = \"fixed\" requires run_size"
                    );
                    continue;
                };
                let text = parse_string(value)
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{line}: value of {key}"))?;
                let bytes = crate::domain::disk::parse_size(&text).ok().filter(|n| *n > 0)
                    .with_context(|| format!("{CONFIG_FILE_NAME}:{line}: value of {key}: expected a positive size such as \"20GiB\", not {text:?}"))?;
                if key == "run_size" {
                    config.disk.run_size = Some(bytes);
                } else {
                    config.disk.landing_size = Some(bytes);
                }
            }
            DiskEstimate::Recent => {
                if let Some((_, line)) = value {
                    tracing::warn!(
                        "{CONFIG_FILE_NAME}:{line}: [disk] {key} is ignored with estimate = \"recent\""
                    );
                }
            }
        }
    }
    if config
        .landing_verification
        .as_ref()
        .is_some_and(|c| c.non_incremental)
    {
        let profiles = Toolchains::resolve(&crate::domain::toolchain::BUILTIN, &config.toolchains)
            .map_err(anyhow::Error::msg)?;
        landing_verification::non_incremental(config.landing_verification.as_ref(), &profiles)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(config)
}

/// The `model` or `effort` (`key`) of a review agent's table on line
/// `number`, `rest` its value: a string that is not blank. Whether its
/// job's provider takes it is checked as the review reads the table
/// ([`ReviewAgents::launch_problems`]).
fn launch_value(key: &str, rest: &str, number: usize) -> Result<String> {
    let value = parse_string(rest.trim())
        .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
    ensure!(
        !value.trim().is_empty(),
        "{CONFIG_FILE_NAME}:{number}: {key} is blank"
    );
    Ok(value)
}

/// The last `[toolchains.<name>]` read, once its table ended: its
/// `profile` is required (a blank one was refused as it was read).
fn finish_toolchain(toolchains: &[ToolchainDeclaration]) -> Result<()> {
    if let Some(last) = toolchains.last()
        && last.profile.is_empty()
    {
        bail!(
            "{CONFIG_FILE_NAME}:{}: [{TOOLCHAINS_PREFIX}{}] has no {TOOLCHAIN_PROFILE}",
            last.line,
            last.name
        );
    }
    Ok(())
}

/// A `[verify_marks.<name>]` as read so far: its header's line, its name
/// and each key's value, checked whole by [`Self::finish`].
#[derive(Default)]
struct MarkTable {
    line: usize,
    name: String,
    class: Option<MarkClass>,
    pattern: Option<LinePattern>,
}

impl MarkTable {
    fn entry(&mut self, number: usize, key: &str, value: &str) -> Result<()> {
        let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
        let defined = match key {
            VERIFY_MARK_CLASS => self.class.is_some(),
            VERIFY_MARK_LINE => self.pattern.is_some(),
            _ => bail!(
                "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{VERIFY_MARKS_PREFIX}{}]; the keys are {VERIFY_MARK_CLASS} and {VERIFY_MARK_LINE}",
                self.name
            ),
        };
        ensure!(
            !defined,
            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
        );
        let value = parse_string(value).with_context(with)?;
        if key == VERIFY_MARK_CLASS {
            self.class = Some(
                MarkClass::parse(&value)
                    .map_err(anyhow::Error::msg)
                    .with_context(with)?,
            );
        } else {
            self.pattern = Some(
                LinePattern::parse(&value)
                    .map_err(anyhow::Error::msg)
                    .with_context(with)?,
            );
        }
        Ok(())
    }

    /// The mark, once its table ended: both keys are required.
    fn finish(self) -> Result<ProjectMark> {
        let missing = |key| {
            anyhow::anyhow!(
                "{CONFIG_FILE_NAME}:{}: [{VERIFY_MARKS_PREFIX}{}] has no {key}",
                self.line,
                self.name
            )
        };
        Ok(ProjectMark {
            class: self.class.ok_or_else(|| missing(VERIFY_MARK_CLASS))?,
            line: self
                .pattern
                .clone()
                .ok_or_else(|| missing(VERIFY_MARK_LINE))?,
            name: self.name,
        })
    }
}

/// The globs of `key = rest` of line `number`: a one-line array of
/// repository-relative globs, not empty, each kept once.
fn nonempty_globs(number: usize, key: &str, rest: &str) -> Result<Vec<String>> {
    let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
    let globs = parse_string_array(rest.trim()).with_context(with)?;
    ensure!(
        !globs.is_empty(),
        "{CONFIG_FILE_NAME}:{number}: {key} names no glob"
    );
    validate_path_globs(&globs).with_context(with)?;
    Ok(dedup_globs(&globs))
}

/// A `[local_checks.gates.<name>]` as read so far, checked whole by
/// [`Self::finish`].
#[derive(Default)]
struct GateTable {
    line: usize,
    name: String,
    paths: Option<Vec<String>>,
    kind: Option<String>,
}

impl GateTable {
    fn entry(&mut self, number: usize, key: &str, rest: &str) -> Result<()> {
        let defined = match key {
            LOCAL_GATE_PATHS => self.paths.is_some(),
            LOCAL_GATE_KIND => self.kind.is_some(),
            _ => bail!(
                "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{LOCAL_GATES_PREFIX}{}]; the keys are {LOCAL_GATE_PATHS} and {LOCAL_GATE_KIND}",
                self.name
            ),
        };
        ensure!(
            !defined,
            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
        );
        if key == LOCAL_GATE_PATHS {
            self.paths = Some(nonempty_globs(number, key, rest)?);
        } else {
            let kind = parse_string(rest.trim())
                .with_context(|| format!("{CONFIG_FILE_NAME}:{number}: value of {key}"))?;
            ensure!(
                !kind.trim().is_empty(),
                "{CONFIG_FILE_NAME}:{number}: {key} of [{LOCAL_GATES_PREFIX}{}] is blank",
                self.name
            );
            self.kind = Some(kind);
        }
        Ok(())
    }

    /// The gate, once its table ended: both keys are required.
    fn finish(self) -> Result<LocalGate> {
        let missing = |key| {
            anyhow::anyhow!(
                "{CONFIG_FILE_NAME}:{}: [{LOCAL_GATES_PREFIX}{}] has no {key}",
                self.line,
                self.name
            )
        };
        Ok(LocalGate {
            paths: self
                .paths
                .clone()
                .ok_or_else(|| missing(LOCAL_GATE_PATHS))?,
            kind: self.kind.clone().ok_or_else(|| missing(LOCAL_GATE_KIND))?,
            name: self.name,
        })
    }
}

/// What the review judges the local gates by in the `dagq.toml` text
/// `text` of the landing branch's commit (ADR-t2589-1 decision 6): its
/// gates ([`LOCAL_GATES_PREFIX`]) and its toolchain profiles resolved
/// against the built-in ones (none while it declares none, and then no
/// profile tells a test's lines apart).
pub fn gate_config(text: &str) -> Result<GateConfig> {
    gate_config_in(text, &crate::domain::toolchain::BUILTIN)
}

/// [`gate_config`] with the profiles of `registry`.
pub fn gate_config_in(text: &str, registry: &ProfileRegistry) -> Result<GateConfig> {
    let config = parse_config(text)?;
    Ok(GateConfig {
        gates: config.local_gates,
        toolchains: Toolchains::resolve(registry, &config.toolchains)
            .map_err(anyhow::Error::msg)?,
    })
}

/// A `[review.programs.<name>]` as read so far: its header's line and
/// name and each key's value, checked whole by [`Self::finish`].
#[derive(Default)]
struct ProgramTable {
    line: usize,
    name: String,
    keys: Vec<String>,
    script: Option<String>,
    args: Option<Vec<String>>,
    paths: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

impl ProgramTable {
    /// Read `key = rest` of line `number`.
    fn entry(&mut self, number: usize, key: &str, rest: &str) -> Result<()> {
        ensure!(
            key != "command",
            "{CONFIG_FILE_NAME}:{number}: [{REVIEW_PROGRAMS_PREFIX}{}] has command, which is not read: write the program as a repository script with script (and args), which runs from the landing branch's commit and execs an outside tool",
            self.name
        );
        ensure!(
            REVIEW_PROGRAM_KEYS.contains(&key),
            "{CONFIG_FILE_NAME}:{number}: unknown key {key} in [{REVIEW_PROGRAMS_PREFIX}{}]; the keys are {}",
            self.name,
            REVIEW_PROGRAM_KEYS.join(", ")
        );
        ensure!(
            !self.keys.iter().any(|existing| existing == key),
            "{CONFIG_FILE_NAME}:{number}: {key} is defined twice"
        );
        self.keys.push(key.to_owned());
        let with = || format!("{CONFIG_FILE_NAME}:{number}: value of {key}");
        match key {
            "script" => {
                let path = parse_string(rest).with_context(with)?;
                review_programs::check_script_path(&path)
                    .map_err(anyhow::Error::msg)
                    .with_context(with)?;
                self.script = Some(path);
            }
            "args" => self.args = Some(parse_string_array(rest).with_context(with)?),
            "paths" => {
                let globs = parse_string_array(rest).with_context(with)?;
                ensure!(
                    !globs.is_empty(),
                    "{CONFIG_FILE_NAME}:{number}: {key} of [{REVIEW_PROGRAMS_PREFIX}{}] names no glob",
                    self.name
                );
                validate_path_globs(&globs).with_context(with)?;
                self.paths = Some(dedup_globs(&globs));
            }
            _ => {
                let secs = parse_positive(rest, "number of seconds").with_context(with)?;
                self.timeout_secs = Some(u64::try_from(secs).with_context(with)?);
            }
        }
        Ok(())
    }

    /// The program the table names: `script` and `paths` are required.
    fn finish(self) -> Result<ReviewProgram> {
        let Self { line, name, .. } = &self;
        let table = format!("{CONFIG_FILE_NAME}:{line}: [{REVIEW_PROGRAMS_PREFIX}{name}]");
        Ok(ReviewProgram {
            script: self
                .script
                .with_context(|| format!("{table} has no script"))?,
            args: self.args.unwrap_or_default(),
            paths: self
                .paths
                .with_context(|| format!("{table} has no paths"))?,
            timeout_secs: self.timeout_secs,
            name: self.name,
            builtin: None,
        })
    }
}

/// A key as written: bare, or a TOML string (`"src-domain"`).
fn parse_key(key: &str) -> Result<String> {
    if key.starts_with(['"', '\'']) {
        parse_string(key)
    } else {
        Ok(key.to_owned())
    }
}

/// Whether `dir` may be a directory of a container a volume is mounted at:
/// absolute, not `/`, without the characters that separate a mount's parts
/// and without `..`, as the mount itself checks it
/// ([`crate::domain::run_container::worker_run_args`]), so a directory
/// read is never refused only when the container is made.
fn is_container_dir(dir: &str) -> bool {
    dir.starts_with('/') && dir != "/" && !dir.contains([',', ':']) && !dir.contains("..")
}

/// A one-line array of TOML strings (`["src/**", 'docs/**']`), followed by
/// nothing but an optional comment.
pub(super) fn parse_string_array(text: &str) -> Result<Vec<String>> {
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
pub(super) fn parse_bool(text: &str) -> Result<bool> {
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

/// `[disk]` of the `dagq.toml` in `root` (ADR-0047 decision 44,
/// ADR-t4473-1), `None` when there is no file; no table or no key is the
/// default ([`DiskConfig`] names each key, its type and default).
/// `reserve` takes a share written as `"5%"`; a bad value is an error.
/// `estimate` is `"recent"` (default) or `"fixed"`; unknown values are errors.
/// Recent supplies W and I from the WORKER and INTEGRATOR peaks, each
/// falling back to the one recent run estimate without peaks
/// ([`crate::domain::disk::stage_budgets`]).
/// Fixed optionally reads `landing_size`, a positive size string for I;
/// omission uses W from `run_size`. Invalid or zero values are
/// line-numbered errors.
/// Recent warns and ignores it, including unreadable values.
/// Fixed requires `run_size`, a positive string size read by
/// [`crate::domain::disk::parse_size`]; absent, invalid and zero values are
/// errors with line numbers. Recent warns and ignores any `run_size` value,
/// regardless of key order. Both callers use the method in [`DiskConfig`].
/// The supervisor warns of a file it cannot read, using defaults at startup
/// and retaining settings in use on later passes. A retired key ([`DiskConfig::RETIRED_KEYS`])
/// is read on without its value, warned of once in a process
/// ([`warns_of_retired_disk_keys`]). An older binary refuses the keys it
/// does not know, so a new key goes into `dagq.toml` only once the
/// installed binary reads it.
pub fn load_disk_config(root: &Path) -> Result<Option<DiskConfig>> {
    load_disk_config_at(root, config_now_millis())
}

pub(super) fn load_disk_config_at(root: &Path, now: i64) -> Result<Option<DiskConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config_at(&path, now)? else {
        return Ok(None);
    };
    let config = parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
    if !config.disk_retired.is_empty() && warns_of_retired_disk_keys() {
        tracing::warn!(
            keys = %config.disk_retired.join(", "),
            "{}: [{DISK_TABLE}] {} are read no more and their values are not used: the disk's thresholds follow [{DISK_TABLE}] reserve and the run expectation with no factor; remove them",
            path.display(),
            config.disk_retired.join(", ")
        );
    }
    Ok(Some(config.disk))
}

/// Whether this call is the first of the process to warn of the retired
/// keys of `[disk]`: the supervisor reads the table again and again, and
/// the warning is told once.
fn warns_of_retired_disk_keys() -> bool {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    !WARNED.swap(true, std::sync::atomic::Ordering::SeqCst)
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

/// `[retention]` of the `dagq.toml` in `root` (ADR-t697-1); no file, no
/// table or no key is the default.
pub fn load_retention_config(root: &Path) -> Result<RetentionConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RetentionConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .retention)
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

/// `[kpi]` of the `dagq.toml` in `root` (ADR-0051 decision 17), with the
/// Codex prices of its `[eval.providers.codex]`
/// ([`crate::infrastructure::kpi_config::with_codex_prices`]); `None` when
/// there is no file, or neither a `[kpi]` table nor prices that convert.
pub fn load_kpi_settings(root: &Path) -> Result<Option<KpiSettings>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            let config =
                parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
            crate::infrastructure::kpi_config::with_codex_prices(
                config.kpi,
                config.eval.provider(crate::domain::Provider::Codex),
            )
        }
        None => None,
    })
}

/// `[recheck]` of the `dagq.toml` in `root`; no file, no table or no key
/// is the default (no command, no paths).
pub fn load_recheck_config(root: &Path) -> Result<RecheckConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RecheckConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .recheck)
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
///
/// The optional `admission_stall_passes` key is a nonnegative u32. Omission
/// uses 5. Attention opens when the streak of startable passes without a
/// start exceeds this count.
pub fn load_supervisor_config(root: &Path) -> Result<Option<SupervisorConfig>> {
    load_supervisor_config_at(root, config_now_millis())
}

pub(crate) fn load_supervisor_config_at(root: &Path, now: i64) -> Result<Option<SupervisorConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config_at(&path, now)? else {
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

/// `[fresh_session]` of the `dagq.toml` in `root` (ADR-t2080-1), `None`
/// when there is no file; no table or no key starts no new session.
pub fn load_fresh_session(root: &Path) -> Result<Option<FreshSessionConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .fresh_session,
    ))
}

/// `[throughput_review]` of the `dagq.toml` in `root`, `None` when there
/// is no file; no table or no key is the default (`kpi_stats` on).
pub fn load_throughput_review(root: &Path) -> Result<Option<ThroughputReviewConfig>> {
    load_throughput_review_at(root, config_now_millis())
}

pub(super) fn load_throughput_review_at(
    root: &Path,
    now: i64,
) -> Result<Option<ThroughputReviewConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config_at(&path, now)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .throughput_review,
    ))
}

/// `[replan]` of the `dagq.toml` in `root`, `None` when there is no
/// file; no table keeps the runtime's detection and diagnosis off, and a
/// key not set keeps its default ([`ReplanConfig`]).
pub fn load_replan(root: &Path) -> Result<Option<ReplanConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .replan,
    ))
}

/// `[review.jobs]` of the `dagq.toml` in `root` (ADR-t1895-1 decision
/// 1), `None` when there is no file; no table or no key keeps each kind's
/// timeout the provider's review timeout.
pub fn load_review_jobs(root: &Path) -> Result<Option<ReviewJobs>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .review_jobs,
    ))
}

/// `[eval]` and `[eval.providers.<provider>]` of the `dagq.toml` in `root`
/// (ADR-t1728-1), `None` when there is no file; no table or no key keeps
/// each default.
pub fn load_eval_config(root: &Path) -> Result<Option<EvalConfig>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(Some(
        parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .eval,
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

/// `[landing_verification]` of the `dagq.toml` in `root` (ADR-t1925-1
/// decision 4); no file or no table is none, which runs a task's
/// verification commands as registered.
pub fn load_landing_verification(root: &Path) -> Result<Option<LandingVerification>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .landing_verification)
}

/// `[verification]` of the `dagq.toml` in `root`; no file, no table or no
/// key leaves the runtime's limit of one verification command.
pub fn load_verification_config(root: &Path) -> Result<VerificationConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(VerificationConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .verification)
}

/// The toolchain profiles the `dagq.toml` in `root` declares
/// ([`TOOLCHAINS_PREFIX`]), resolved against `registry`, with the
/// project's own marks it declares ([`VERIFY_MARKS_PREFIX`]) and its
/// `[telemetry]` and `[[telemetry.commands]]`
/// ([`crate::infrastructure::telemetry_config`]): no file or no
/// declaration is no profile (ADR-t2268-1 decision 3), and a profile
/// `registry` does not have is a mistake of the file.
pub fn load_toolchains(root: &Path, registry: &ProfileRegistry) -> Result<Toolchains> {
    let path = root.join(CONFIG_FILE_NAME);
    let (declarations, marks, telemetry) = match read_config(&path)? {
        Some(text) => {
            let config =
                parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
            (config.toolchains, config.verify_marks, config.telemetry)
        }
        None => Default::default(),
    };
    Ok(Toolchains::resolve(registry, &declarations)
        .map_err(anyhow::Error::msg)?
        .with_marks(marks)
        .with_telemetry(telemetry))
}

/// The `[toolchains.<name>]` the `dagq.toml` in `root` declares, as
/// written: none without the file. What `dagq init` proposes from
/// ([`crate::domain::toolchain::recommend::propose`]).
pub fn load_toolchain_declarations(root: &Path) -> Result<Vec<ToolchainDeclaration>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .toolchains
        }
        None => Vec::new(),
    })
}

/// The profiles the runtime reads a worker's commands' output with for its
/// measures (the tests its Claude transcript and its Codex turns name as
/// failed): the `dagq.toml` of `checkout`, the main checkout of the
/// repository the queue is bound to (`None`: bound to none; `Err`: not
/// found, why), read as the landing's verification reads it
/// ([`load_toolchains`] on the built-in profiles) and taken by
/// [`Toolchains::for_measures`]; why one cannot be read is logged.
pub fn measured_toolchains(checkout: Option<Result<&Path, &str>>) -> Toolchains {
    let loaded = checkout.map(|checkout| {
        checkout
            .map_err(|why| anyhow::anyhow!("no main checkout: {why}"))
            .and_then(|checkout| load_toolchains(checkout, &crate::domain::toolchain::BUILTIN))
            .inspect_err(|error| {
                tracing::info!(
                    error = %format_args!("{error:#}"),
                    "the failed tests of the worker's commands not read: {error:#}"
                );
            })
    });
    Toolchains::for_measures(loaded)
}

/// The profiles the readers outside the landing read with (the CI watch's
/// test names, the host's load, a claim's toolchain record and the KPIs'
/// `toolchain` axis): those [`load_toolchains`] resolves against
/// `registry`, or, when `dagq.toml` cannot be read or names a profile
/// `registry` does not have, no profile, as the measures read
/// ([`Toolchains::for_measures`]): a broken file does not say which
/// profile applies, so none is guessed. The landing stops on the mistake,
/// and `doctor` and `status` show it.
pub fn toolchains_in_use(root: &Path, registry: &ProfileRegistry) -> Toolchains {
    Toolchains::for_measures(Some(load_toolchains(root, registry)))
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

/// `[tasks] title_prefixes` of the `dagq.toml` in `root`, `None` when
/// there is no file or no such key.
pub fn load_title_prefixes(root: &Path) -> Result<Option<TitlePrefixes>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => {
            parse_config(&text)
                .with_context(|| format!("parse {}", path.display()))?
                .title_prefixes
        }
        None => None,
    })
}

/// `[tasks] related_docs` of the `dagq.toml` in `root` (ADR-t3321-1),
/// empty when there is no file or no such key.
pub fn load_related_docs(root: &Path) -> Result<Vec<String>> {
    let path = root.join(CONFIG_FILE_NAME);
    Ok(match read_config(&path)? {
        Some(text) => parse_config(&text)
            .with_context(|| format!("parse {}", path.display()))?
            .related_docs
            .unwrap_or_default(),
        None => Vec::new(),
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

/// `[goals] max_spike_cycles` of the `dagq.toml` in `root` (ADR-t1487-1
/// decision 8); no file, no table or no key is none.
pub fn load_max_spike_cycles(root: &Path) -> Result<Option<usize>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .max_spike_cycles)
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

/// Read the main checkout's working file for each e2e start. Missing
/// configuration is no declaration; read and parse failures stay errors.
pub fn load_e2e_command(root: &Path) -> Result<Option<crate::application::install::E2eCommand>> {
    let path = root.join(CONFIG_FILE_NAME);
    read_config(&path)?
        .map(|text| parse_config(&text).map(|config| config.e2e_command))
        .transpose()
        .map(Option::flatten)
        .with_context(|| format!("read e2e command of {}", path.display()))
}

/// `[[recurring]]` of the `dagq.toml` in `root` (ADR-0072 decision 1); no
/// file or no table is none.
/// The supervisor's planner pass rereads the main checkout's working file.
/// A parse error is an error of the whole file with its line; the pass
/// registers nothing, warns once per error and never opens a decision ask.
/// Doctor exposes it as recurring.error. Status/stats recurring.schedule
/// show last registration, unfinished task and next due time via
/// crate::domain::recurring::RecurringSchedule.
pub fn load_recurring(root: &Path) -> Result<Vec<crate::domain::recurring::RecurringDraft>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .recurring)
}

/// `[egress] registries` of the `dagq.toml` in `root`
/// ([`EGRESS_REGISTRIES`]); no file, no table or no key is none.
pub fn load_egress_registries(root: &Path) -> Result<Vec<RegistryHost>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(Vec::new());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .egress_registries)
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

/// What the `dagq.toml` in `root` gives the review agent `agent`'s launch
/// ([`crate::application::agent_eval::LaunchSettings`]): `[roles.*]`, what
/// its `[review.subagents.<agent>]` writes and the providers
/// `[eval.providers.*]` declares; none of them without the file.
pub fn load_eval_launch_settings(
    root: &Path,
    agent: &str,
) -> Result<crate::application::agent_eval::LaunchSettings> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(crate::application::agent_eval::LaunchSettings::default());
    };
    let config = parse_config(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(crate::application::agent_eval::LaunchSettings {
        table: config.review_agents().written_launch(agent),
        declared: config.eval.declared_providers,
        roles: config.roles,
    })
}

/// `[roles.inbox]` of the `dagq.toml` in `root` (ADR-t1181-1 decision 1),
/// read alone: the file with every line outside that table blanked
/// ([`inbox_table_only`]), parsed and checked as the whole file is. So a
/// mistake of that table (an unknown provider, a Claude model for Codex,
/// an unknown key) is an error and one elsewhere in the file (a table a
/// newer binary knows) is not: `dagq inbox` refuses the first and is not
/// kept closed by the second. No file is no table.
pub fn load_inbox_role(root: &Path) -> Result<RoleModels> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RoleModels::default());
    };
    Ok(parse_config(&inbox_table_only(&text))
        .with_context(|| format!("parse {}", path.display()))?
        .roles)
}

/// `text` with every line blanked but those of its `[roles.inbox]` table
/// (its header to the next header, a header being a line that starts with
/// `[` as [`parse_config`] reads one), so the line numbers stay those of
/// the file.
fn inbox_table_only(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut inside = false;
    text.lines()
        .map(|raw| {
            let line = raw.trim();
            if let Some(header) = line.strip_prefix('[') {
                let name = strip_comment(header).trim();
                inside = name.strip_suffix(']').map(str::trim)
                    == Some(&format!("{ROLES_PREFIX}inbox")[..]);
            }
            if inside { raw } else { "" }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `runtime` of `[roles.worker]` and `[roles.verification]` of the
/// `dagq.toml` in `root` (goal 38); no file is the host for both.
/// Read afresh: the supervisor calls this each pass, and integrate for each
/// invocation, against the main checkout. Status, doctor and the session
/// wrapper read through `BoundRuntimes`, resolving the queue's checkout.
pub fn load_runtimes(root: &Path) -> Result<RuntimeConfig> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(RuntimeConfig::default());
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .runtimes)
}

/// `[container.image]` of the `dagq.toml` in `root`, the main checkout
/// (ADR-t2257-1 decision 2); no file or no table is `None`, the base
/// alone.
pub fn load_project_image(root: &Path) -> Result<Option<ProjectImage>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .container_image)
}

/// `[container.verification] prepare` of the `dagq.toml` in `root`
/// ([`CONTAINER_VERIFICATION_TABLE`]); `None` without the file or the
/// table.
pub fn load_verification_prepare(root: &Path) -> Result<Option<String>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    Ok(parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))?
        .container_verification_prepare)
}

/// `[container.build_output] dir` of the `dagq.toml` in `root`, the main
/// checkout ([`CONTAINER_BUILD_OUTPUT_TABLE`]); no file or no table is
/// `None`, no build output volume.
pub fn load_build_output_dir(root: &Path) -> Result<Option<String>> {
    Ok(load_container_config(root)?.and_then(|config| config.container_build_output))
}

/// `[container.worker_cache] dirs` of the `dagq.toml` in `root`, the main
/// checkout ([`CONTAINER_WORKER_CACHE_TABLE`]); no file or no table is
/// empty, which mounts no worker cache.
pub fn load_worker_cache_dirs(root: &Path) -> Result<Vec<String>> {
    Ok(load_container_config(root)?
        .map(|config| config.container_worker_cache)
        .unwrap_or_default())
}

/// The whole `dagq.toml` in `root`, `None` without the file.
fn load_container_config(root: &Path) -> Result<Option<Config>> {
    let path = root.join(CONFIG_FILE_NAME);
    let Some(text) = read_config(&path)? else {
        return Ok(None);
    };
    parse_config(&text)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

/// The runtimes of the main checkout a queue is bound to (goal 38), read
/// again on each [`Self::read`]: `checkout` is the binding as resolved. A
/// queue bound to no checkout runs every target on the host; a binding
/// that could not be resolved is an error of every read, so a caller that
/// must not run on the host by mistake (the wrapper's turn) refuses.
pub struct BoundRuntimes {
    checkout: std::result::Result<Option<PathBuf>, String>,
}

impl BoundRuntimes {
    pub fn new(checkout: Result<Option<PathBuf>>) -> Self {
        Self {
            checkout: checkout.map_err(|error| format!("{error:#}")),
        }
    }

    /// The runtimes as the checkout's `dagq.toml` sets them now
    /// ([`load_runtimes`]).
    pub fn read(&self) -> Result<RuntimeConfig> {
        match &self.checkout {
            Ok(Some(checkout)) => load_runtimes(checkout),
            Ok(None) => Ok(RuntimeConfig::default()),
            Err(error) => bail!("the checkout the queue is bound to could not be found: {error}"),
        }
    }
}

/// `"host"` or `"container"`.
fn parse_runtime(text: &str) -> Result<Runtime> {
    let value = parse_string(text)?;
    value
        .parse::<Runtime>()
        .map_err(|_| anyhow::anyhow!("unknown runtime {value:?} (\"host\" or \"container\")"))
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

/// The most bytes of a `dagq.toml` dagq reads: the queue service reads the
/// main checkout's with its own rights, and a larger one is refused rather
/// than read on.
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// The uncommitted operational layer beside the main checkout's config.
pub const LOCAL_CONFIG_FILE_NAME: &str = "dagq.local.toml";
/// A local layer may expire at most seven days (604800 seconds) after a read.
pub const LOCAL_CONFIG_MAX_SECS: i64 = 7 * 24 * 60 * 60;

/// Only operational tables whose readers reread while running are admitted.
/// supervisor: ClaimState::reread_limits each pass; disk/stall/resume:
/// Supervisor::reread_operational_rules before resources and claims each pass.
/// conflicts: Supervisor::reread_conflicts; provider_fallback:
/// Supervisor::reread_provider_fallback; fresh_session:
/// Supervisor::reread_fresh_session; replan: Supervisor::reread_replan, all
/// each pass. throughput_review: each review invocation; kpi and kpi.targets:
/// each KPI calculation and finding-planner proposal check. ci_watch:
/// observation::ci_watch_pass rereads each pass. retention: HostOps::remove_expired
/// and planning::remove_expired_dirs read each sweep. Landing recheck settings
/// are excluded because they control a gate before landing.
/// Review, verification, authorization, audit, worker inputs,
/// repository destinations and planning vocabulary are never admitted.
const LOCAL_TABLES: &[&str] = &[
    "supervisor",
    "disk",
    "stall",
    "resume",
    "conflicts",
    "provider_fallback",
    "fresh_session",
    "replan",
    "throughput_review",
    "kpi",
    "ci_watch",
    "retention",
];

fn config_now_millis() -> i64 {
    crate::application::unix_millis(std::time::SystemTime::now())
}

/// File-based load_* readers share read_config_at; this entry supplies the
/// wall clock. Only a main checkout can
/// supply a local layer; a linked run worktree is excluded by the adapter.
/// Committed-tree readers (GitRepository::file_in, review configuration,
/// setup_review and remote repository rules) call parse_config on blob text,
/// never this function, and therefore cannot see the local file.
fn read_config(path: &Path) -> Result<Option<String>> {
    read_config_at(path, config_now_millis())
}

fn read_config_at(path: &Path, now: i64) -> Result<Option<String>> {
    match path.parent() {
        Some(root) => local_snapshot(root, now).text,
        None => read_config_file(path),
    }
}

/// One read of the main checkout `root`'s settings at `now`: what the
/// layer is ([`LocalLayerRead`]) and the configuration text in effect
/// from the same bytes (the merged text while the layer is used, the
/// committed file's otherwise, the committed file's error when it cannot
/// be read: a local layer never repairs a bad base).
struct LocalSnapshot {
    read: LocalLayerRead,
    text: Result<Option<String>>,
}

fn local_snapshot(root: &Path, now: i64) -> LocalSnapshot {
    let base = read_config_file(&root.join(CONFIG_FILE_NAME));
    let rejected = |reason: String, hash: Option<String>| LocalLayerRead {
        state: LocalLayerState::rejected(reason),
        hash,
        expires_at: None,
        keys: Vec::new(),
        values: Default::default(),
    };
    // The file is read once, with the committed file's checks (a FIFO, a
    // device or a file over the size is not read on), and hashed whether
    // or not the layer is used, so an edit of a rejected layer is a change.
    let local = match read_config_bytes(&root.join(LOCAL_CONFIG_FILE_NAME)) {
        Ok(None) => {
            return LocalSnapshot {
                read: LocalLayerRead::absent(),
                text: base,
            };
        }
        Ok(Some(bytes)) => bytes,
        Err(error) => {
            return LocalSnapshot {
                read: rejected(format!("{error:#}"), None),
                text: base,
            };
        }
    };
    let hash = Some(crate::domain::replan_snapshot::sha256(&local));
    let base = match base {
        Ok(base) => base,
        Err(error) => {
            return LocalSnapshot {
                read: rejected(
                    format!("the committed dagq.toml cannot be read: {error:#}"),
                    hash,
                ),
                text: Err(error),
            };
        }
    };
    let mut read = LocalLayerRead {
        state: LocalLayerState::Effective,
        hash,
        expires_at: None,
        keys: Vec::new(),
        values: Default::default(),
    };
    let merged = (|| -> Result<String> {
        let base_text = base.as_deref().unwrap_or("");
        parse_config(base_text).context("the committed dagq.toml does not parse")?;
        ensure!(
            super::adapters::local_config_is_untracked(root)?,
            "local config is tracked or outside the main checkout"
        );
        let local = String::from_utf8(local).context("local config is not UTF-8")?;
        merge_local_config(base_text, &local, now, &mut read)
    })();
    match merged {
        Ok(text) => LocalSnapshot {
            read,
            text: Ok(Some(text)),
        },
        Err(error) => {
            if read.state == LocalLayerState::Effective {
                read.state = LocalLayerState::rejected(format!("{error:#}"));
            }
            read.keys.clear();
            LocalSnapshot {
                read,
                text: Ok(base),
            }
        }
    }
}

/// What a read of the main checkout `root`'s `dagq.local.toml` at `now`
/// finds ([`LocalLayerRead`]): the same read [`read_config_at`] uses, for
/// `status`, `doctor` and the audit's settings.
pub fn inspect_local_config(root: &Path, now: i64) -> LocalLayerRead {
    local_snapshot(root, now).read
}

/// The local supervisor's operational settings and the layer's read from
/// one read of the files each pass, so the values applied and the record
/// of the layer are of the same bytes. An absent file/table is the
/// defaults; a read error is left to the loop, which retains the settings
/// in use and warns once per distinct error.
pub fn load_operational_snapshot(
    root: &Path,
    clock: &dyn crate::application::Clock,
) -> (
    Result<crate::application::supervise::repository_config::OperationalRules>,
    LocalLayerRead,
) {
    let snapshot = local_snapshot(root, crate::application::unix_millis(clock.system_time()));
    let rules = snapshot.text.and_then(|text| {
        let config = match text {
            Some(text) => parse_config(&text)?,
            None => Config::default(),
        };
        Ok(
            crate::application::supervise::repository_config::OperationalRules {
                stall: config.stall,
                disk: config.disk,
                resume: config.resume,
            },
        )
    });
    (rules, snapshot.read)
}

/// Whether Git ignores the main checkout `root`'s `dagq.local.toml`, as
/// `git check-ignore` run at the root judges a file there, read without
/// Git (the queue service runs none). The sources in Git's order of
/// precedence: the root `.gitignore` (not followed when a symlink, as Git
/// does not follow one in the working tree), then `info/exclude` of the
/// Git directory, then `core.excludesFile` (a relative path is of the
/// root; unset, `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`).
/// The first source with a matching pattern decides, by its last match (a
/// `!` pattern unignores). Patterns follow gitignore and Git's wildmatch:
/// `*`, `?`, `[...]` with ranges and `[:class:]` names (an unknown name or
/// an unclosed `[` matches nothing), `**`, `\` escapes, an anchoring `/`,
/// a trailing `/` for directories only, and the case folding of
/// `core.ignoreCase`. The `core` settings are the last set of the user's
/// configs (`$XDG_CONFIG_HOME/git/config` or `~/.config/git/config`, then
/// `~/.gitconfig`) and the repository's, read as Git reads them
/// ([`GitCore`]); Git's system config is not read. Each file is read as
/// `dagq.toml` is ([`read_git_input`]: a regular file, opened without
/// blocking, of at most [`MAX_CONFIG_BYTES`], or `/dev/null`): one that is
/// not, a config Git refuses, or an include whose condition this does not
/// evaluate, is the error (whether Git ignores the layer is then not
/// known), which `doctor` shows. `doctor` warns of a layer this finds not
/// ignored: a `git add -A` would commit it, and a tracked layer is never
/// used.
pub fn local_config_ignored(root: &Path) -> Result<bool> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    local_config_ignored_with(root, home.as_deref(), xdg.as_deref())
}

fn local_config_ignored_with(root: &Path, home: Option<&Path>, xdg: Option<&Path>) -> Result<bool> {
    let dot = root.join(".git");
    let git_dir = if fs::metadata(&dot).is_ok_and(|metadata| metadata.is_dir()) {
        dot
    } else {
        let gitdir = read_git_input(&dot, true)?.and_then(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            Some(root.join(text.trim().strip_prefix("gitdir: ")?.trim()))
        });
        gitdir.unwrap_or(dot)
    };
    let xdg = xdg
        .filter(|xdg| !xdg.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|home| home.join(".config")));
    // The user's configs, then the repository's: the last to set a key wins.
    let mut configs = Vec::new();
    if let Some(xdg) = &xdg {
        configs.push(xdg.join("git/config"));
    }
    if let Some(home) = home {
        configs.push(home.join(".gitconfig"));
    }
    configs.push(git_dir.join("config"));
    let mut core = GitCore::default();
    let place = GitPlace {
        home,
        git_dir: &git_dir,
    };
    for config in &configs {
        core.read_file(config, &place, 0)?;
    }
    let excludes = match core.excludes_file.as_deref() {
        // An empty value names no file, and no default either.
        Some("") => None,
        Some(file) => Some(match file.strip_prefix("~/") {
            Some(rest) => home.map_or_else(|| PathBuf::from(file), |home| home.join(rest)),
            None => root.join(file),
        }),
        None => xdg.map(|xdg| xdg.join("git/ignore")),
    };
    let sources = [
        (root.join(".gitignore"), false),
        (git_dir.join("info/exclude"), true),
    ]
    .into_iter()
    .chain(excludes.map(|file| (file, true)));
    for (source, follow) in sources {
        let Some(text) = read_git_input(&source, follow)? else {
            continue;
        };
        if let Some(ignored) =
            last_ignore_match(&text, LOCAL_CONFIG_FILE_NAME.as_bytes(), core.ignore_case)
        {
            return Ok(ignored);
        }
    }
    Ok(false)
}

/// A file the ignore check reads, by [`read_bounded`]; `/dev/null` (or a
/// path that resolves to it, with `follow`) is an empty file, as Git
/// reads it where a person names it to have no ignore file.
fn read_git_input(path: &Path, follow: bool) -> Result<Option<Vec<u8>>> {
    if follow && fs::canonicalize(path).is_ok_and(|real| real == Path::new("/dev/null")) {
        return Ok(Some(Vec::new()));
    }
    read_bounded(path, follow)
}

/// What a Git config's includes are judged by: the home for `~/`, and
/// the Git directory that `includeIf "gitdir:"` and `"onbranch:"` match.
struct GitPlace<'a> {
    home: Option<&'a Path>,
    git_dir: &'a Path,
}

/// The `core` settings of Git's configs that decide what the ignore files
/// say, the last to set each winning: `excludesFile` and `ignoreCase`.
#[derive(Debug, Default, PartialEq)]
struct GitCore {
    excludes_file: Option<String>,
    ignore_case: bool,
}

/// The section of a Git config line: `[core]`, `[include]`, an
/// `[includeIf]` whose condition holds, or another.
#[derive(Clone, Copy, PartialEq)]
enum GitSection {
    Core,
    Include,
    Other,
}

impl GitCore {
    /// Takes the config file at `path` (none when absent), following its
    /// includes at the point of each, up to Git's depth of 10.
    fn read_file(&mut self, path: &Path, place: &GitPlace, depth: usize) -> Result<()> {
        ensure!(
            depth <= 10,
            "{}: includes nested deeper than Git follows",
            path.display()
        );
        let Some(bytes) = read_git_input(path, true)? else {
            return Ok(());
        };
        self.read(&String::from_utf8_lossy(&bytes), path, place, depth)
    }

    /// Takes the settings of the Git config `text` of the file `path` as
    /// Git reads it: a UTF-8 BOM skipped; the keys of `[core]`, a key
    /// following its header on the line; a key without `=` true; each
    /// value of any section by [`git_config_value`], a line Git refuses
    /// being the error; `[include] path` and `[includeIf "..."] path`
    /// (relative to `path`'s directory, `~/` of the home) read where they
    /// stand, for the conditions `gitdir:`, `gitdir/i:` and `onbranch:`;
    /// `hasconfig:` is not evaluated and is the error; another condition
    /// never holds, as in Git. Booleans by [`git_config_bool`];
    /// `excludesFile` without its `:(optional)` prefix.
    fn read(&mut self, text: &str, path: &Path, place: &GitPlace, depth: usize) -> Result<()> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut section = GitSection::Other;
        let mut lines = text.lines().enumerate();
        while let Some((number, line)) = lines.next() {
            let bad = |why: &str| anyhow::anyhow!("{}:{}: {why}", path.display(), number + 1);
            let mut line = line.trim_start();
            if let Some(header) = line.strip_prefix('[') {
                let (name, rest) = git_section(header).ok_or_else(|| bad("bad section header"))?;
                section = match name {
                    (name, None) if name.eq_ignore_ascii_case("core") => GitSection::Core,
                    (name, None) if name.eq_ignore_ascii_case("include") => GitSection::Include,
                    (name, Some(condition)) if name.eq_ignore_ascii_case("includeif") => {
                        if git_include_holds(&condition, path, place)? {
                            GitSection::Include
                        } else {
                            GitSection::Other
                        }
                    }
                    _ => GitSection::Other,
                };
                line = rest.trim_start();
            }
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            let (key, value) = match line.split_once('=') {
                Some((key, value)) => (
                    key.trim(),
                    Some(git_config_value(value, &mut lines).map_err(bad)?),
                ),
                None => (line.split([' ', '\t', '#', ';']).next().unwrap_or(""), None),
            };
            match section {
                GitSection::Core if key.eq_ignore_ascii_case("excludesfile") => {
                    let value = value.ok_or_else(|| bad("core.excludesFile without a value"))?;
                    let value = value.strip_prefix(":(optional)").unwrap_or(&value);
                    self.excludes_file = Some(value.to_owned());
                }
                GitSection::Core if key.eq_ignore_ascii_case("ignorecase") => {
                    self.ignore_case = git_config_bool(value.as_deref())
                        .ok_or_else(|| bad("core.ignoreCase is not a boolean"))?;
                }
                GitSection::Include if key.eq_ignore_ascii_case("path") => {
                    let value = value.ok_or_else(|| bad("include.path without a value"))?;
                    let file = match value.strip_prefix("~/") {
                        Some(rest) => place.home.map(|home| home.join(rest)),
                        None => Some(path.parent().unwrap_or(Path::new("/")).join(&value)),
                    };
                    if let Some(file) = file {
                        self.read_file(&file, place, depth + 1)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// The section of the Git config header `header` (the text after its
/// `[`): its name and its quoted subsection (with `\` escapes), and the
/// text after the `]`; `None` when it does not read.
fn git_section(header: &str) -> Option<((&str, Option<String>), &str)> {
    let end = header.find([' ', '\t', '"', ']'])?;
    let name = &header[..end];
    let rest = header[end..].trim_start_matches([' ', '\t']);
    if let Some(rest) = rest.strip_prefix(']') {
        return Some(((name, None), rest));
    }
    let mut chars = rest.strip_prefix('"')?.char_indices();
    let mut subsection = String::new();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => {
                let rest = rest[1 + at + 1..].strip_prefix(']')?;
                return Some(((name, Some(subsection)), rest));
            }
            '\\' => subsection.push(chars.next()?.1),
            c => subsection.push(c),
        }
    }
    None
}

/// Whether `[includeIf "condition"]` of the config `path` holds: `gitdir:`
/// (`gitdir/i:` without case) matches the Git directory, as it is and as
/// resolved, with Git's pattern (`~/` of the home, `./` of `path`'s
/// directory, `**/` before a relative one, `**` after a trailing `/`);
/// `onbranch:` the branch `HEAD` names. `hasconfig:` is the error: its
/// answer needs remotes this check does not read.
fn git_include_holds(condition: &str, path: &Path, place: &GitPlace) -> Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let starstar = |mut pattern: String| {
        if pattern.ends_with('/') {
            pattern.push_str("**");
        }
        pattern
    };
    let (gitdir, fold) = match (
        condition.strip_prefix("gitdir:"),
        condition.strip_prefix("gitdir/i:"),
    ) {
        (Some(pattern), _) => (Some(pattern), false),
        (_, Some(pattern)) => (Some(pattern), true),
        _ => (None, false),
    };
    if let Some(pattern) = gitdir {
        let pattern = if let Some(rest) = pattern.strip_prefix("~/") {
            let Some(home) = place.home else {
                return Ok(false);
            };
            format!("{}/{rest}", home.display())
        } else if let Some(rest) = pattern.strip_prefix("./") {
            let dir = fs::canonicalize(path)
                .ok()
                .and_then(|real| real.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            format!("{}/{rest}", dir.display())
        } else if pattern.starts_with('/') {
            pattern.to_owned()
        } else {
            format!("**/{pattern}")
        };
        let pattern = starstar(pattern);
        let dirs = [
            fs::canonicalize(place.git_dir).ok(),
            Some(place.git_dir.to_path_buf()),
        ];
        return Ok(dirs.iter().flatten().any(|dir| {
            gitignore_wildmatch(pattern.as_bytes(), dir.as_os_str().as_bytes(), true, fold)
        }));
    }
    if let Some(pattern) = condition.strip_prefix("onbranch:") {
        let head = read_git_input(&place.git_dir.join("HEAD"), true)?.unwrap_or_default();
        let head = String::from_utf8_lossy(&head);
        let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") else {
            return Ok(false);
        };
        let pattern = starstar(pattern.to_owned());
        return Ok(gitignore_wildmatch(
            pattern.as_bytes(),
            branch.as_bytes(),
            true,
            false,
        ));
    }
    ensure!(
        !condition.starts_with("hasconfig:"),
        "{}: includeIf \"{condition}\" is not a condition this check evaluates",
        path.display()
    );
    Ok(false)
}

/// A Git config value whose text after the `=` is `text`, as Git reads
/// it: outside quotes a `;` or `#` begins a comment and whitespace at
/// either end goes (within it stays as written); `"` quotes; `\` escapes
/// `"`, `\`, `n`, `t` and `b`, and at the end of a line joins the next of
/// `more`. An unclosed quote or another escape is the error Git gives.
fn git_config_value<'a>(
    mut text: &'a str,
    more: &mut impl Iterator<Item = (usize, &'a str)>,
) -> std::result::Result<String, &'static str> {
    let mut value = String::new();
    let mut quoted = false;
    // Where the value ends when what follows is only unquoted whitespace.
    let mut trim = None;
    loop {
        let mut chars = text.chars();
        let mut joined = false;
        while let Some(c) = chars.next() {
            match c {
                ';' | '#' if !quoted => break,
                ' ' | '\t' | '\r' if !quoted => {
                    if !value.is_empty() {
                        trim.get_or_insert(value.len());
                        value.push(c);
                    }
                }
                '"' => {
                    trim = None;
                    quoted = !quoted;
                }
                '\\' => {
                    trim = None;
                    match chars.next() {
                        None => {
                            joined = true;
                            break;
                        }
                        Some('n') => value.push('\n'),
                        Some('t') => value.push('\t'),
                        Some('b') => value.push('\u{8}'),
                        Some(c @ ('\\' | '"')) => value.push(c),
                        Some(_) => return Err("bad config line: an unknown escape"),
                    }
                }
                c => {
                    trim = None;
                    value.push(c);
                }
            }
        }
        match joined.then(|| more.next()).flatten() {
            Some((_, line)) => text = line,
            None => break,
        }
    }
    if quoted {
        return Err("bad config line: an unclosed quote");
    }
    if let Some(len) = trim {
        value.truncate(len);
    }
    Ok(value)
}

/// A Git config boolean as Git reads it: no value, `true`, `yes` and `on`
/// true; empty, `false`, `no` and `off` false (in any case); else an
/// integer (decimal, `0x` hex or `0` octal, with a `k`, `m` or `g` unit)
/// true unless zero; `None` when it is none of these.
fn git_config_bool(value: Option<&str>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" | "" => Some(false),
        number => {
            // A unit or a sign does not change whether it is zero.
            let number = number.trim_start();
            let digits = number.strip_suffix(['k', 'm', 'g']).unwrap_or(number);
            let digits = digits.strip_prefix(['-', '+']).unwrap_or(digits);
            let magnitude = if let Some(hex) = digits.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).ok()?
            } else if digits.len() > 1
                && let Some(octal) = digits.strip_prefix('0')
            {
                u64::from_str_radix(octal, 8).ok()?
            } else {
                digits.parse::<u64>().ok()?
            };
            Some(magnitude != 0)
        }
    }
}

/// Whether the last pattern of the gitignore `text` that matches the file
/// `path` at its directory ignores it (`Some(false)` for a `!` pattern),
/// `None` when none matches. Lines as Git reads them: a UTF-8 BOM at the
/// start skipped, a `\r` before the `\n` dropped, a line beginning with
/// `#` a comment, a NUL ending the pattern (Git reads it as a C string),
/// trailing spaces trimmed unless escaped. `fold`: the case folding of
/// `core.ignoreCase`.
fn last_ignore_match(text: &[u8], path: &[u8], fold: bool) -> Option<bool> {
    let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
    let mut decided = None;
    for line in text.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let line = line.split(|byte| *byte == 0).next().unwrap_or(line);
        let pattern = trim_trailing_spaces(line);
        let (negated, pattern) = match pattern.strip_prefix(b"!") {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        // A directory's pattern never matches a file.
        if pattern.is_empty() || pattern.ends_with(b"/") {
            continue;
        }
        let pattern = pattern.strip_prefix(b"/").unwrap_or(pattern);
        if gitignore_wildmatch(pattern, path, true, fold) {
            decided = Some(!negated);
        }
    }
    decided
}

/// `line` without its trailing spaces, as Git trims a gitignore line: a
/// space a `\` escapes stays, and so does all of a line ending in a lone
/// `\`.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space = None;
    let mut at = 0;
    while at < line.len() {
        match line[at] {
            b' ' => {
                last_space.get_or_insert(at);
            }
            b'\\' => {
                at += 1;
                if at == line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        at += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

/// Git's wildmatch with paths: `*` and `?` do not match `/`, `**` between
/// slashes (or at either end) matches any number of directories, `[...]`
/// a class ([`gitignore_class`]), `\` escapes the next byte (a trailing
/// one matches nothing). `segment` says the pattern starts a path
/// segment. Under `fold` a byte of the text and a literal one of the
/// pattern compare in lowercase; an escaped byte and a class's own bytes
/// are not folded, as in Git. It is matched without recursion, as the set
/// of the text's positions the pattern so far reaches: each element of the
/// pattern is one step over at most `text.len() + 1` positions, so a
/// pattern of any length (a line of a `.gitignore` of up to
/// [`MAX_CONFIG_BYTES`]) takes work of its length times the text's and no
/// stack beyond this frame.
fn gitignore_wildmatch(pattern: &[u8], text: &[u8], segment: bool, fold: bool) -> bool {
    let lower = |c: u8| if fold { c.to_ascii_lowercase() } else { c };
    let n = text.len();
    // reached[at]: the pattern so far matches `text[..at]`.
    let mut reached = vec![false; n + 1];
    reached[0] = true;
    let mut segment = segment;
    let mut rest = pattern;
    // The positions one byte on from `reached` where `matches` takes it.
    let step = |reached: &[bool], matches: &dyn Fn(u8) -> bool| {
        let mut next = vec![false; n + 1];
        for at in 0..n {
            next[at + 1] = reached[at] && matches(text[at]);
        }
        next
    };
    loop {
        if !reached.contains(&true) {
            return false;
        }
        let Some((&first, after)) = rest.split_first() else {
            return reached[n];
        };
        match first {
            b'*' => {
                let stars = rest.iter().take_while(|c| **c == b'*').count();
                let after = &rest[stars..];
                if stars >= 2 && segment && (after.is_empty() || after[0] == b'/') {
                    if after.is_empty() {
                        return true;
                    }
                    // `**/`: no directory, or any number of them.
                    let mut before = false;
                    let mut next = vec![false; n + 1];
                    for at in 0..=n {
                        next[at] = reached[at] || (before && text[at - 1] == b'/');
                        before |= reached[at];
                    }
                    reached = next;
                    rest = &after[1..];
                    segment = true;
                } else {
                    // A run of stars not between slashes is one `*`.
                    let mut next = vec![false; n + 1];
                    for at in 0..=n {
                        next[at] = reached[at] || (at > 0 && next[at - 1] && text[at - 1] != b'/');
                    }
                    reached = next;
                    rest = after;
                    segment = false;
                }
            }
            b'?' => {
                reached = step(&reached, &|c| c != b'/');
                rest = after;
                segment = false;
            }
            b'[' => {
                // The class's extent does not depend on the byte it takes.
                let Some((_, past)) = gitignore_class(after, 0, fold) else {
                    return false;
                };
                let class = &after[..after.len() - past.len()];
                reached = step(&reached, &|c| {
                    c != b'/' && gitignore_class(class, lower(c), fold).is_some_and(|(hit, _)| hit)
                });
                rest = past;
                segment = false;
            }
            b'\\' => {
                let Some((&escaped, past)) = after.split_first() else {
                    return false;
                };
                reached = step(&reached, &|c| lower(c) == escaped);
                rest = past;
                segment = false;
            }
            c => {
                reached = step(&reached, &|t| lower(t) == lower(c));
                rest = after;
                segment = c == b'/';
            }
        }
    }
}

/// Git's `[...]` whose bytes after its `[` are `class`, for the text byte
/// `c` (lowercased under `fold`): whether it matches, and the pattern after
/// its `]`; `None` when it is unclosed or names an unknown `[:class:]`,
/// which Git takes as no match of the whole pattern. As Git's wildmatch: a
/// leading `!` or `^` negates, a `]` first is itself, `x-y` is a range
/// (under `fold`, of `c`'s uppercase too) unless a range or a `[:class:]`
/// is just before the `-` or a `]` just after it, `\` escapes,
/// `[:name:]` is an ASCII class (under `fold`, `upper` takes lowercase
/// too), and a `[:` without its `:]` before the next `]` is a `[`.
fn gitignore_class(class: &[u8], c: u8, fold: bool) -> Option<(bool, &[u8])> {
    let (negated, mut p) = match class.split_first() {
        Some((b'!' | b'^', rest)) => (true, rest),
        _ => (false, class),
    };
    let mut matched = false;
    // The byte a following `-` makes a range from.
    let mut prev = None;
    let mut first = true;
    loop {
        let (&ch, rest) = p.split_first()?;
        if ch == b']' && !first {
            return Some((matched != negated, rest));
        }
        first = false;
        p = rest;
        match ch {
            b'\\' => {
                let (&escaped, rest) = p.split_first()?;
                p = rest;
                matched |= escaped == c;
                prev = Some(escaped);
            }
            b'-' if prev.is_some() && p.first().is_some_and(|end| *end != b']') => {
                let (mut end, mut rest) = (p[0], &p[1..]);
                if end == b'\\' {
                    (end, rest) = rest.split_first().map(|(end, rest)| (*end, rest))?;
                }
                p = rest;
                let start = prev.take().unwrap_or_default();
                let within = |c: u8| (start..=end).contains(&c);
                matched |=
                    within(c) || (fold && c.is_ascii_lowercase() && within(c.to_ascii_uppercase()));
            }
            b'[' if p.first() == Some(&b':') => {
                let named = &p[1..];
                let close = named.iter().position(|byte| *byte == b']')?;
                if close == 0 || named[close - 1] != b':' {
                    matched |= c == b'[';
                    prev = Some(b'[');
                    continue;
                }
                matched |= match &named[..close - 1] {
                    b"alnum" => c.is_ascii_alphanumeric(),
                    b"alpha" => c.is_ascii_alphabetic(),
                    b"blank" => matches!(c, b' ' | b'\t'),
                    b"cntrl" => c.is_ascii_control(),
                    b"digit" => c.is_ascii_digit(),
                    b"graph" => c.is_ascii_graphic(),
                    b"lower" => c.is_ascii_lowercase(),
                    b"print" => c.is_ascii_graphic() || c == b' ',
                    b"punct" => c.is_ascii_punctuation(),
                    b"space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r'),
                    b"upper" => c.is_ascii_uppercase() || (fold && c.is_ascii_lowercase()),
                    b"xdigit" => c.is_ascii_hexdigit(),
                    _ => return None,
                };
                p = &named[close + 1..];
                prev = None;
            }
            _ => {
                matched |= ch == c;
                prev = Some(ch);
            }
        }
    }
}

/// A value of a line as [`crate::domain::local_config::LocalKey`] shows
/// it: a string's quotes normalized, other values without their comment.
fn shown_value(text: &str) -> String {
    let text = text.trim();
    if text.starts_with(['\'', '"'])
        && let Ok(value) = parse_string(text)
    {
        return format!("{value:?}");
    }
    value_without_comment(text).to_owned()
}

/// The text of a value without its comment: from a `#` outside a string,
/// the strings (`'...'`, and `"..."` with its `\` escapes, in an array
/// too) kept whole, so that a comment of any length is not part of what
/// the layer's read and record keep of the value.
fn value_without_comment(text: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if c == '#' => return text[..at].trim(),
            None if matches!(c, '\'' | '"') => quote = Some(c),
            None => {}
        }
    }
    text.trim()
}

/// Merge individual (table, key) lines, then validate the complete result
/// with the same parser as dagq.toml. This permits partial tables whose
/// required companion keys come from the base, and rejects unknown keys,
/// duplicate keys, invalid values and cross-table inconsistencies atomically.
/// The required `[local] expires_at` metadata is an RFC 3339 instant, valid
/// after this read and no later than LOCAL_CONFIG_MAX_SECS from it; it is
/// not included in the merged configuration. `read` gets the expiry, the
/// state `expired` when it passed, and the keys with their committed
/// values.
fn merge_local_config(
    base: &str,
    local: &str,
    now: i64,
    read: &mut LocalLayerRead,
) -> Result<String> {
    use std::collections::{BTreeMap, BTreeSet};
    let mut entries = BTreeMap::new();
    let mut tables = BTreeSet::new();
    let mut table = "";
    let mut expiry = None;
    for raw in local.strip_prefix('\u{feff}').unwrap_or(local).lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            table = strip_comment(header)
                .strip_suffix(']')
                .context("unclosed local table")?
                .trim();
            ensure!(
                table == "local"
                    || LOCAL_TABLES.contains(&table)
                    || table.starts_with("kpi.targets."),
                "local table not allowed: {table}"
            );
            ensure!(tables.insert(table), "duplicate local table");
        } else {
            let (key, value) = line.split_once('=').context("missing local assignment")?;
            let key = parse_key(key.trim())?;
            // Keys are emitted as bare names below. Validate after decoding:
            // a quoted key's escapes must never become assignments or headers.
            ensure!(
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
                "local keys must be nonempty ASCII bare names"
            );
            if table == "local" {
                ensure!(
                    key == "expires_at" && expiry.is_none(),
                    "unknown or duplicate local metadata"
                );
                expiry = Some(
                    crate::domain::stats::rfc3339_millis(&parse_string(value.trim())?)
                        .context("invalid local expiry")?,
                );
            } else {
                ensure!(!table.is_empty(), "local key outside table");
                ensure!(
                    entries
                        .insert(
                            (table.to_owned(), key),
                            value_without_comment(value).to_owned()
                        )
                        .is_none(),
                    "duplicate local key"
                );
            }
        }
    }
    let expiry = expiry.context("missing local.expires_at")?;
    read.expires_at = Some(expiry);
    if expiry <= now {
        read.state = LocalLayerState::Expired;
        bail!("local config expired");
    }
    ensure!(
        expiry.saturating_sub(now) <= LOCAL_CONFIG_MAX_SECS * 1000,
        "local expiry outside permitted interval"
    );
    let values = config_value_comparer(base, &entries);
    let local_entries = entries.clone();
    let (merged, mut committed) = substitute(base, entries);
    parse_config(&merged)?;
    read.keys = local_entries
        .into_iter()
        .map(|((table, key), value)| {
            let committed = committed
                .remove(&(table.clone(), key.clone()))
                .map(|text| value_without_comment(&text).to_owned());
            let redundant = values.same(&table, &key, Some(&value), committed.as_deref());
            crate::domain::local_config::LocalKey {
                committed: committed.as_deref().map(shown_value),
                committed_text: committed,
                local: shown_value(&value),
                local_text: value,
                redundant,
                table,
                key,
            }
        })
        .collect();
    read.values = values;
    Ok(merged)
}

/// `base` with each (table, key) of `entries` set to its value text: a
/// key's line replaced where the table has one, else inserted after the
/// table's header, else appended in a new table (parse_config rejects a
/// repeated table). With the texts of the lines it replaced.
fn substitute(
    base: &str,
    mut entries: std::collections::BTreeMap<(String, String), String>,
) -> (String, std::collections::BTreeMap<(String, String), String>) {
    let mut replaced = std::collections::BTreeMap::new();
    let mut merged = String::new();
    let mut current = "";
    for raw in base.strip_prefix('\u{feff}').unwrap_or(base).lines() {
        let line = raw.trim();
        if let Some(header) = line.strip_prefix('[') {
            current = strip_comment(header).strip_suffix(']').unwrap_or("").trim();
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some((key, base_value)) = line.split_once('=')
            && let Ok(key) = parse_key(key.trim())
            && let Some(value) = entries.remove(&(current.to_owned(), key.clone()))
        {
            replaced.insert((current.to_owned(), key.clone()), base_value.to_owned());
            merged.push_str(&format!("{key} = {value}\n"));
            continue;
        }
        merged.push_str(raw);
        merged.push('\n');
    }
    for ((table, key), value) in entries {
        let header = format!("[{table}]");
        let lines: Vec<_> = merged.lines().collect();
        let at = lines.iter().position(|line| {
            line.trim()
                .strip_prefix('[')
                .and_then(|h| strip_comment(h).strip_suffix(']'))
                .is_some_and(|name| name.trim() == table)
        });
        if let Some(at) = at {
            let mut lines = lines
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>();
            lines.insert(at + 1, format!("{key} = {value}"));
            merged = lines.join("\n") + "\n";
        } else {
            merged.push_str(&format!("{header}\n{key} = {value}\n"));
        }
    }
    (merged, replaced)
}

/// The [`crate::domain::local_config::ValueComparer`] of a read over the
/// committed `base` with the layer's `entries`: two texts of a key are one
/// value when the configuration in effect with each of them set for the
/// key parses to the same configuration. That configuration is `base`
/// with the layer's other keys over it, so a key is compared as the
/// configuration the layer makes uses it (a `[disk] run_size` that the
/// committed `estimate = 'recent'` ignores counts where the layer sets
/// `estimate = 'fixed'`), and setting a key to its committed text is
/// removing the layer's line for it. Each key's own parser decides
/// (`'5%'` and `'5.0%'`, `'8GiB'` and `'8192MiB'`, `1` and `1.0` where
/// the key reads a number). A text the parser refuses is compared as
/// written.
fn config_value_comparer(
    base: &str,
    entries: &std::collections::BTreeMap<(String, String), String>,
) -> crate::domain::local_config::ValueComparer {
    let base = base.to_owned();
    let entries = entries.clone();
    crate::domain::local_config::ValueComparer::new(move |table, key, a, b| {
        let parsed = |text: &str| {
            let mut entries = entries.clone();
            entries.insert((table.to_owned(), key.to_owned()), text.trim().to_owned());
            parse_config(&substitute(&base, entries).0).ok()
        };
        match (parsed(a), parsed(b)) {
            (Some(a), Some(b)) => a == b,
            _ => a.trim() == b.trim(),
        }
    })
}

/// The text of the `dagq.toml` at `path`; none when there is none. Only a
/// regular file (a symlink is followed) of at most [`MAX_CONFIG_BYTES`] is
/// read: a device, a FIFO or a directory, and a larger file, are refused
/// with why, so that a `dagq.toml` that is a symlink to `/dev/zero` cannot
/// make the reader take memory without end. The file is opened without
/// blocking, so that one swapped for a FIFO after the check does not hang.
fn read_config_file(path: &Path) -> Result<Option<String>> {
    read_config_bytes(path)?
        .map(|bytes| {
            String::from_utf8(bytes).with_context(|| format!("read {}: not UTF-8", path.display()))
        })
        .transpose()
}

/// The bytes [`read_config_file`] reads, with its checks (a regular file,
/// opened without blocking, at most [`MAX_CONFIG_BYTES`]) but not its
/// UTF-8 one: the local layer's hash is of these. A path under a file
/// that is not a directory is no file, as a missing one. The layer's
/// tracked check reads the `.git` file and the Git config with it.
pub(super) fn read_config_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    read_bounded(path, true)
}

/// [`read_config_bytes`] of `path`; with `follow` false a symlink there is
/// no file (it is neither followed nor read), as Git takes a symlinked
/// `.gitignore` in the working tree.
fn read_bounded(path: &Path, follow: bool) -> Result<Option<Vec<u8>>> {
    read_regular(path, follow, Some(MAX_CONFIG_BYTES))
}

/// The bytes of Git's index at `path` for the layer's tracked check, with
/// the checks of [`read_config_bytes`] but no limit to the size, which is
/// the repository's.
pub(super) fn read_index_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    read_regular(path, true, None)
}

/// The bytes of the regular file at `path` (followed through a symlink
/// with `follow`, else a symlink is no file), opened without blocking so
/// that one swapped for a FIFO after the check does not hang; none when
/// absent; a device, a FIFO, a directory or a file over `limit` bytes is
/// refused with why.
fn read_regular(path: &Path, follow: bool, limit: Option<u64>) -> Result<Option<Vec<u8>>> {
    use std::{io::ErrorKind, io::Read, os::unix::fs::OpenOptionsExt};
    let not_regular = |path: &Path| format!("read {}: not a regular file", path.display());
    let metadata = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    match metadata {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(None),
        Ok(metadata) => ensure!(metadata.is_file(), not_regular(path)),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | nofollow)
        .open(path)
        .with_context(|| format!("read {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(metadata.is_file(), not_regular(path));
    let mut bytes = Vec::new();
    file.take(limit.map_or(u64::MAX, |limit| limit + 1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if let Some(limit) = limit {
        ensure!(
            bytes.len() as u64 <= limit,
            "read {}: larger than the {limit} bytes dagq reads of it",
            path.display()
        );
    }
    Ok(Some(bytes))
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
///
/// Callers supply the main checkout, never the run's worktree.
/// Provision reads before creating the worktree and appends these pairs
/// after worker identity variables to the background wrapper's environment;
/// a bad file fails provisioning, starts no wrapper and holds claims.
/// Review passes the pairs to its headless command. Integrate and the
/// already-satisfied snapshot check pass them to verification commands;
/// other receipt validation runs no commands. With no verification command
/// they do not read this table. Host verification (including flaky retries
/// and the snapshot check) replaces TMPDIR with a short per-run directory
/// outside runs/; container verification does not.
/// A read error aborts landing and restores the run's previous state.
/// Resume reads before spending an attempt and appends the pairs after the
/// same worker identity variables (including the actor name). Its session
/// continues the worker's workspace for ADR-0049 decision 3, so it needs
/// the same compile cache and build/test concurrency. An unreadable file
/// leaves needs_session and spends no attempt; the landing-branch read
/// checks the same file first. Missing programs hold new resumes as claims.
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
    /// killed and recorded as a `timeout` failure (task 639) when the
    /// `dagq.toml` of `checkout` declares no `[verification]
    /// command_timeout_secs`, which replaces it
    /// ([`Verifier::verification_timeout`]):
    /// [`VERIFICATION_TIMEOUT`](super::adapters::VERIFICATION_TIMEOUT)
    /// but in tests, which shorten it to reach the limit.
    pub verification_timeout: std::time::Duration,
    /// The profiles a `[toolchains.<name>]` may name:
    /// [`crate::domain::toolchain::BUILTIN`] but in tests that add one.
    pub profiles: &'static ProfileRegistry,
    /// Where a command run for a run is that run's process while it runs
    /// ([`Verifier::run_to_log_for`]): the supervisor's measure of the
    /// runs' resources reads them.
    pub children: crate::domain::environment::RunChildren,
}

impl Verifier for ShellVerifier {
    fn settings_snapshot(
        &self,
    ) -> Result<Option<crate::domain::audit::administration::SettingsSnapshot>> {
        super::audit::settings_snapshot(&self.checkout).map(Some)
    }

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

    fn recheck_config(&self) -> Result<RecheckConfig> {
        load_recheck_config(&self.checkout)
    }

    fn worker_trial(&self) -> Result<WorkerTrial> {
        load_worker_trial(&self.checkout)
    }
    fn runtimes(&self) -> Result<RuntimeConfig> {
        load_runtimes(&self.checkout)
    }
    fn landing_batch_size(&self) -> Result<std::num::NonZeroUsize> {
        Ok(load_verification_config(&self.checkout)?.landing_batch_size)
    }
    fn landing_verification(&self) -> Result<Option<LandingVerification>> {
        load_landing_verification(&self.checkout)
    }
    fn verification_timeout(&self) -> Result<std::time::Duration> {
        Ok(load_verification_config(&self.checkout)?.command_timeout(self.verification_timeout))
    }
    fn prepare_verification(&self, materials: &Path, log: &Path) -> Result<Option<Exit>> {
        let Some(command) = load_verification_prepare(&self.checkout)? else {
            return Ok(None);
        };
        let env = [(
            crate::domain::run_container::MATERIALS_ENV.to_owned(),
            path_str(materials)?.to_owned(),
        )];
        self.run_to_log(&command, &self.checkout, &env, log)
            .map(Some)
    }
    fn toolchains(&self) -> Result<Toolchains> {
        load_toolchains(&self.checkout, self.profiles)
    }
    fn ci_watch_config(&self) -> Result<Option<CiWatchConfig>> {
        load_ci_watch(&self.checkout)
    }
    fn retention_config(&self) -> Result<RetentionConfig> {
        load_retention_config(&self.checkout)
    }
    fn e2e_paths(&self) -> Result<Vec<String>> {
        load_e2e_paths(&self.checkout)
    }
    fn review_subagents_in(&self, text: &str) -> Result<ReviewAgents> {
        Ok(parse_config(text)?.review_agents())
    }
    fn review_code_paths_in(&self, text: &str) -> Result<Option<CodePaths>> {
        Ok(parse_config(text)?.review_code)
    }
    fn review_revise_limit_in(&self, text: &str) -> Result<Option<ReviseLimit>> {
        Ok(parse_config(text)?.review_revise_limit)
    }
    fn review_programs_in(&self, text: &str) -> Result<ReviewPrograms> {
        Ok(parse_config(text)?.review_programs())
    }
    fn headless_wrapper_setting(&self) -> Result<Option<HeadlessWrapper>> {
        load_headless_wrapper_setting(&self.checkout)
    }
    fn role_models(&self) -> Result<RoleModels> {
        load_role_models(&self.checkout)
    }
    fn eval_config(&self) -> Result<EvalConfig> {
        Ok(load_eval_config(&self.checkout)?.unwrap_or_default())
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
        let timeout = Verifier::verification_timeout(self)?;
        super::adapters::run_shell_to_log(command, cwd, env, log, timeout).map(super::process::exit)
    }

    fn run_to_log_for(
        &self,
        run: &crate::domain::RunId,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        log: &Path,
    ) -> Result<Exit> {
        let timeout = Verifier::verification_timeout(self)?;
        super::adapters::run_shell_to_log_started(command, cwd, env, log, timeout, &mut |pid| {
            self.children.started(run, pid)
        })
        .map(super::process::exit)
    }
}

/// A main read or parse failure blocks landing; it must never be interpreted
/// as an empty set of conditions. Diagnostics do not expose upstream contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandingConfigError;
impl std::fmt::Display for LandingConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("landing blocked: main configuration unavailable or invalid")
    }
}
impl std::error::Error for LandingConfigError {}

/// Read main's commit-pinned dagq.toml using the existing runner-App read port.
/// No checkout path, parsed run Config, or cached fallback is accepted. A failed
/// read/parse stops landing even if an earlier main snapshot was valid. `now`
/// is supplied by the caller's clock for token issuance. Not wired into landing.
pub fn read_main_approval_conditions(
    source: &dyn crate::application::supervise::repository_config::MainRules,
    repository_name: &str,
    now: i64,
) -> std::result::Result<crate::domain::landing_approval::MainApprovalConditions, LandingConfigError>
{
    use crate::application::supervise::repository_config::read_scope;
    let token = source
        .issue(&read_scope(repository_name), now)
        .map_err(|_| LandingConfigError)?;
    let commit = source.main_sha(&token).map_err(|_| LandingConfigError)?;
    let text = source
        .contents(&token, &commit)
        .map_err(|_| LandingConfigError)?;
    let config = parse_config(&text).map_err(|_| LandingConfigError)?;
    Ok(
        crate::domain::landing_approval::MainApprovalConditions::from_main(
            commit,
            config.landing_approval,
        ),
    )
}

#[cfg(test)]
mod tests {
    const LOCAL_NOW: i64 = 1_800_000_000_000;
    const LOCAL_VALID: &str = "[local]\nexpires_at = '2027-01-16T08:00:00Z'\n";

    /// A main checkout without external Git: an unborn repository has no index.
    fn local_checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[throughput_review]\nkpi_stats = true\n[disk]\nreserve = '5%'\nestimate = 'fixed'\nrun_size = '8 GiB'\n").unwrap();
        dir
    }

    #[test]
    fn local_layer_merges_keys_and_rereads_without_restarting() {
        let dir = local_checkout();
        let path = dir.path().join(CONFIG_FILE_NAME);
        let local = dir.path().join(LOCAL_CONFIG_FILE_NAME);
        // The fixed instant is 2027-01-15T08:00:00Z; expiry is one day later.
        let read = || parse_config(&read_config_at(&path, LOCAL_NOW).unwrap().unwrap()).unwrap();
        assert!(read().throughput_review.kpi_stats);
        fs::write(
            &local,
            format!(
                "{LOCAL_VALID}[throughput_review]\nkpi_stats = false\n[disk]\n\"reserve\" = '12%'\n"
            ),
        )
        .unwrap();
        let config = read();
        assert!(!config.throughput_review.kpi_stats);
        assert!(
            !load_throughput_review_at(dir.path(), LOCAL_NOW)
                .unwrap()
                .unwrap()
                .kpi_stats
        );
        assert_eq!(
            load_disk_config_at(dir.path(), LOCAL_NOW)
                .unwrap()
                .unwrap()
                .reserve,
            0.12
        );
        assert_eq!(config.disk.reserve, 0.12);
        assert_eq!(config.disk.estimate, DiskEstimate::Fixed);
        assert_eq!(config.disk.run_size, Some(8 * 1024 * 1024 * 1024));
        fs::remove_file(&local).unwrap();
        assert!(read().throughput_review.kpi_stats);
        assert_eq!(read().disk.reserve, 0.05);
    }

    #[test]
    fn local_layer_fails_closed_and_keeps_base_errors() {
        let dir = local_checkout();
        let path = dir.path().join(CONFIG_FILE_NAME);
        let base = fs::read_to_string(&path).unwrap();
        for local in [
            "[disk]\nreserve = '12%'\n".to_owned(),
            "[local]\nexpires_at = '2027-02-16T08:00:00Z'\n[disk]\nreserve = '12%'".to_owned(),
            "[local]\nexpires_at = '2027-01-15T08:00:00Z'\n[disk]\nreserve = '12%'".to_owned(),
            "[local]\nexpires_at = 'garbage'\n[disk]\nreserve = '12%'".to_owned(),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'\n[repository]\npush = false"),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'\n[run.env]\nX = 'bad'"),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'\nunknown = true"),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'\nreserve = '13%'"),
            format!("{LOCAL_VALID}[disk\nreserve = '12%'"),
            format!("{LOCAL_VALID}[disk]\nreserve = 'broken'"),
        ] {
            fs::write(dir.path().join(LOCAL_CONFIG_FILE_NAME), &local).unwrap();
            assert_eq!(
                read_config_at(&path, LOCAL_NOW).unwrap().unwrap(),
                base,
                "{local}"
            );
        }
        fs::write(
            dir.path().join(LOCAL_CONFIG_FILE_NAME),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'"),
        )
        .unwrap();
        assert_eq!(
            read_config_at(&path, LOCAL_NOW + 86_400_000)
                .unwrap()
                .unwrap(),
            base
        );
        fs::write(&path, "[disk]\nreserve = 'invalid'").unwrap();
        assert!(load_disk_config(dir.path()).is_err());
    }

    #[test]
    fn local_layer_rejects_key_injection_into_forbidden_tables() {
        let dir = local_checkout();
        let path = dir.path().join(CONFIG_FILE_NAME);
        let original = fs::read_to_string(&path).unwrap();
        // Exercise both appending a new allowed table and inserting a key
        // into an existing one. The forbidden assignments themselves are
        // valid config, so rejection cannot pass merely on a bad payload.
        for base in [original.clone(), format!("{original}[retention]\n")] {
            fs::write(&path, &base).unwrap();
            for (key, value, forbidden) in [
                (
                    r#""\n[run.env]\nLD_PRELOAD""#,
                    "'/tmp/x.so'",
                    "[run.env]\nLD_PRELOAD = '/tmp/x.so'\n",
                ),
                (
                    r#""\n[repository]\npush""#,
                    "false",
                    "[repository]\npush = false\n",
                ),
                (
                    r#""\n[verification]\ncommand_timeout_secs""#,
                    "1",
                    "[verification]\ncommand_timeout_secs = 1\n",
                ),
                (
                    r#""\n[worker.trial]\nenabled""#,
                    "false",
                    "[worker.trial]\nenabled = false\n",
                ),
            ] {
                assert!(parse_config(forbidden).is_ok(), "{forbidden}");
                let local = format!(
                    "{LOCAL_VALID}[throughput_review]\nkpi_stats = false\n[retention]\n{key} = {value}\n"
                );
                fs::write(dir.path().join(LOCAL_CONFIG_FILE_NAME), local).unwrap();
                assert_eq!(
                    read_config_at(&path, LOCAL_NOW).unwrap().unwrap(),
                    base,
                    "{key}"
                );
            }
        }
        for key in [
            r#""days\r""#,
            r#""days\t""#,
            r#""days[""#,
            r#""days=7""#,
            r#""""#,
        ] {
            let local = format!("{LOCAL_VALID}[retention]\n{key} = 7\n");
            assert!(
                merge_local_config(&original, &local, LOCAL_NOW, &mut LocalLayerRead::absent())
                    .is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn local_layer_refuses_landing_recheck_settings() {
        let dir = local_checkout();
        let path = dir.path().join(CONFIG_FILE_NAME);
        let base = format!(
            "{}[recheck]\ncommand = 'committed-gate'\npaths = ['src/**']\n",
            fs::read_to_string(&path).unwrap()
        );
        fs::write(&path, &base).unwrap();
        for setting in ["command = 'local-gate'", "paths = ['**']", ""] {
            let local = format!(
                "{LOCAL_VALID}[throughput_review]\nkpi_stats = false\n[recheck]\n{setting}\n"
            );
            fs::write(dir.path().join(LOCAL_CONFIG_FILE_NAME), local).unwrap();
            assert_eq!(read_config_at(&path, LOCAL_NOW).unwrap().unwrap(), base);
        }
    }

    /// The inspection the supervisor records and `status`/`doctor` show:
    /// each key with its committed value and the file's hash and expiry
    /// while used, the expiry passed, a rejection with its reason, and no
    /// file.
    #[test]
    fn local_layer_inspection_reports_keys_expiry_and_rejection() {
        use crate::domain::local_config::LocalLayerState;
        let dir = local_checkout();
        assert_eq!(
            inspect_local_config(dir.path(), LOCAL_NOW),
            LocalLayerRead::absent()
        );
        let text = format!(
            "{LOCAL_VALID}[disk]\nreserve = \"12%\" # trial\n[throughput_review]\nkpi_stats = true\n[retention]\ndays = 3\n"
        );
        fs::write(dir.path().join(LOCAL_CONFIG_FILE_NAME), &text).unwrap();
        let read = inspect_local_config(dir.path(), LOCAL_NOW);
        assert_eq!(read.state, LocalLayerState::Effective);
        assert_eq!(
            read.hash.as_deref(),
            Some(crate::domain::replan_snapshot::sha256(text.as_bytes()).as_str())
        );
        assert_eq!(read.effective_hash(), read.hash.as_deref());
        assert_eq!(read.expires_at, Some(LOCAL_NOW + 86_400_000));
        let keys = read
            .keys
            .iter()
            .map(|key| {
                (
                    format!("{}.{}", key.table, key.key),
                    key.local.as_str(),
                    key.committed.as_deref(),
                    key.redundant(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                ("disk.reserve".to_owned(), "\"12%\"", Some("\"5%\""), false),
                ("retention.days".to_owned(), "3", None, false),
                (
                    "throughput_review.kpi_stats".to_owned(),
                    "true",
                    Some("true"),
                    true
                ),
            ]
        );
        let expired = inspect_local_config(dir.path(), LOCAL_NOW + 86_400_000);
        assert_eq!(expired.state, LocalLayerState::Expired);
        assert_eq!(expired.effective_hash(), None);
        assert!(expired.keys.is_empty());
        fs::write(
            dir.path().join(LOCAL_CONFIG_FILE_NAME),
            format!("{LOCAL_VALID}[repository]\npush = false\n"),
        )
        .unwrap();
        let rejected = inspect_local_config(dir.path(), LOCAL_NOW);
        assert_eq!(
            rejected.state,
            LocalLayerState::Rejected {
                reason: "local table not allowed: repository".into()
            }
        );
        assert!(rejected.hash.is_some());
    }

    /// The layer is read as the committed file is, a regular file of at
    /// most [`MAX_CONFIG_BYTES`] opened without blocking, also when the
    /// committed file cannot be read: a FIFO, `/dev/zero` and a larger
    /// file are rejected without a hash and without being read on.
    #[test]
    fn local_layer_is_read_only_as_a_bounded_regular_file() {
        let dir = local_checkout();
        let local = dir.path().join(LOCAL_CONFIG_FILE_NAME);
        let fifo = dir.path().join("fifo");
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let large = dir.path().join("large");
        fs::write(
            &large,
            vec![b'#'; usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1],
        )
        .unwrap();
        for base_readable in [true, false] {
            if !base_readable {
                fs::write(dir.path().join(CONFIG_FILE_NAME), [0xff, 0xfe]).unwrap();
            }
            for (target, why) in [
                (Path::new("/dev/zero"), "not a regular file"),
                (fifo.as_path(), "not a regular file"),
                (large.as_path(), "larger than the"),
            ] {
                std::os::unix::fs::symlink(target, &local).unwrap();
                let read = inspect_local_config(dir.path(), LOCAL_NOW);
                let LocalLayerState::Rejected { reason } = &read.state else {
                    panic!("{read:?}");
                };
                assert!(reason.contains(why), "{reason}");
                assert_eq!(read.hash, None);
                fs::remove_file(&local).unwrap();
            }
        }
    }

    /// A rejected layer is hashed whatever rejects it, so an edit of it
    /// that keeps the reason is a new read.
    #[test]
    fn a_rejected_layer_is_hashed_so_an_edit_is_seen() {
        let dir = local_checkout();
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[disk\n").unwrap();
        let local = dir.path().join(LOCAL_CONFIG_FILE_NAME);
        let mut hashes = Vec::new();
        for text in ["[disk]\nreserve = '12%'\n", "[disk]\nreserve = '13%'\n"] {
            let text = format!("{LOCAL_VALID}{text}");
            fs::write(&local, &text).unwrap();
            let read = inspect_local_config(dir.path(), LOCAL_NOW);
            let LocalLayerState::Rejected { reason } = &read.state else {
                panic!("{read:?}");
            };
            assert!(
                reason.contains("the committed dagq.toml does not parse"),
                "{reason}"
            );
            assert_eq!(
                read.hash.as_deref(),
                Some(crate::domain::replan_snapshot::sha256(text.as_bytes()).as_str())
            );
            hashes.push((reason.clone(), read.hash));
        }
        assert_eq!(hashes[0].0, hashes[1].0);
        assert_ne!(hashes[0].1, hashes[1].1);
    }

    /// `redundant` and the comparison of committed values agree with the
    /// parser: for each allowed key, with the companion keys its table
    /// needs, the committed spelling in `dagq.toml` and the layer's in
    /// `dagq.local.toml` are one value exactly when `parse_config` reads the
    /// two whole files, written out by hand here, to the same value. Each
    /// type the allowed tables read (integer, string, percentage, size,
    /// string array, a KPI target's number) has a pair the parser reads as
    /// the same and a pair it reads apart; the read's comparer, which a
    /// committed value's change is judged with, gives the same answer.
    #[test]
    fn local_layer_values_compare_as_the_parser_takes_them() {
        type Value = fn(&Config) -> String;
        let stall: Value = |config| format!("{:?}", config.stall.idle_without_receipt_secs);
        let reserve: Value = |config| format!("{:?}", config.disk.reserve);
        let run_size: Value = |config| format!("{:?}", config.disk.run_size);
        let light: Value = |config| {
            format!(
                "{:?}",
                config
                    .supervisor
                    .light_changes()
                    .values()
                    .iter()
                    .map(|change| change.as_str())
                    .collect::<Vec<_>>()
            )
        };
        let target: Value = |config| format!("{:?}", config.kpi.as_ref().map(|kpi| &kpi.targets));
        let tasks = "[tasks]\nchanges = [\"feature\", \"docs\", \"config\"]\n";
        let fixed = "estimate = 'fixed'\n";
        let fix = "change = \"fix\"\n";
        let phase = "kpi.targets.\"phase.work\"";
        // (table, companion lines, key, committed, layer, read, same)
        let cases: Vec<(&str, &str, &str, &str, &str, Value, bool)> = vec![
            (
                "stall",
                "",
                "idle_without_receipt_secs",
                "1000",
                "1_000",
                stall,
                true,
            ),
            (
                "stall",
                "",
                "idle_without_receipt_secs",
                "1_000",
                "1000 # same",
                stall,
                true,
            ),
            (
                "stall",
                "",
                "idle_without_receipt_secs",
                "1000",
                "1001",
                stall,
                false,
            ),
            ("disk", "", "reserve", "'5%'", "\"5%\"", reserve, true),
            ("disk", "", "reserve", "'5%'", "'5.0%'", reserve, true),
            ("disk", "", "reserve", "'5%'", "'6%'", reserve, false),
            (
                "disk",
                fixed,
                "run_size",
                "'8GiB'",
                "'8192MiB'",
                run_size,
                true,
            ),
            (
                "disk", fixed, "run_size", "'8GiB'", "'9GiB'", run_size, false,
            ),
            (
                "supervisor",
                "",
                "light_changes",
                "[\"docs\"]",
                "['docs']",
                light,
                true,
            ),
            (
                "supervisor",
                "",
                "light_changes",
                "[\"docs\"]",
                "[ \"docs\" , ]",
                light,
                true,
            ),
            (
                "supervisor",
                "",
                "light_changes",
                "[\"docs\"]",
                "[\"docs\", \"config\"]",
                light,
                false,
            ),
            (phase, fix, "max", "1", "1.0", target, true),
            (phase, fix, "max", "1", "2", target, false),
        ];
        let dir = local_checkout();
        for (table, companions, key, committed, local, value, same) in cases {
            let file = |text: &str| format!("{tasks}[{table}]\n{companions}{key} = {text}\n");
            // The parser's own reading of each whole file is the baseline.
            let parsed_committed = value(&parse_config(&file(committed)).unwrap());
            let parsed_local = value(&parse_config(&file(local)).unwrap());
            assert_eq!(
                parsed_committed == parsed_local,
                same,
                "{committed} {local}"
            );
            fs::write(dir.path().join(CONFIG_FILE_NAME), file(committed)).unwrap();
            fs::write(
                dir.path().join(LOCAL_CONFIG_FILE_NAME),
                format!("{LOCAL_VALID}[{table}]\n{key} = {local}\n"),
            )
            .unwrap();
            let read = inspect_local_config(dir.path(), LOCAL_NOW);
            assert_eq!(read.state, LocalLayerState::Effective, "{local}: {read:?}");
            let shown = read.keys.iter().find(|shown| shown.key == key).unwrap();
            assert_eq!(shown.redundant(), same, "{committed} {local}: {shown:?}");
            assert_eq!(
                read.values.same(table, key, Some(committed), Some(local)),
                same,
                "{committed} {local}"
            );
            // An unquoted value is shown as written, without its comment.
            if !local.starts_with(['\'', '"']) {
                assert_eq!(
                    shown.local,
                    local.split('#').next().unwrap().trim(),
                    "{shown:?}"
                );
            }
        }
    }

    /// A key is compared in the configuration the layer makes, not in the
    /// committed one alone: where the committed `estimate = 'recent'`
    /// ignores `run_size` and the layer sets `estimate = 'fixed'`, the
    /// layer's `run_size` counts. The baseline is `parse_config` of whole
    /// files written out by hand: the layer's line is redundant exactly when
    /// the merged file and the merged file without the line read the same,
    /// and a landing's change of the committed size is a change the tracker
    /// records once.
    #[test]
    fn a_key_is_compared_in_the_configuration_the_layer_makes() {
        use crate::domain::local_config::{LocalLayerChange, LocalLayerTracker};
        let committed = |size: &str| format!("[disk]\nestimate = 'recent'\nrun_size = {size}\n");
        let merged = |size: &str| format!("[disk]\nestimate = 'fixed'\nrun_size = {size}\n");
        // The committed configuration reads the two sizes the same; the
        // layer's reads them apart.
        assert_eq!(
            parse_config(&committed("'8GiB'")).unwrap(),
            parse_config(&committed("'9GiB'")).unwrap()
        );
        assert_ne!(
            parse_config(&merged("'8GiB'")).unwrap(),
            parse_config(&merged("'9GiB'")).unwrap()
        );
        let dir = local_checkout();
        fs::write(dir.path().join(CONFIG_FILE_NAME), committed("'8GiB'")).unwrap();
        let local = dir.path().join(LOCAL_CONFIG_FILE_NAME);
        for size in ["'9GiB'", "'8192MiB'"] {
            // Without the layer's run_size line, the committed '8GiB' is used.
            let redundant =
                parse_config(&merged(size)).unwrap() == parse_config(&merged("'8GiB'")).unwrap();
            fs::write(
                &local,
                format!("{LOCAL_VALID}[disk]\nestimate = 'fixed'\nrun_size = {size}\n"),
            )
            .unwrap();
            let read = inspect_local_config(dir.path(), LOCAL_NOW);
            assert_eq!(read.state, LocalLayerState::Effective, "{read:?}");
            let key = |name: &str| {
                read.keys
                    .iter()
                    .find(|key| key.key == name)
                    .unwrap()
                    .clone()
            };
            assert_eq!(key("run_size").redundant(), redundant, "{size}");
            assert!(!key("estimate").redundant());
            assert!(
                !read
                    .values
                    .same("disk", "run_size", Some("'8GiB'"), Some("'9GiB'"))
            );
        }
        let mut tracker = LocalLayerTracker::default();
        let first = tracker.observe(inspect_local_config(dir.path(), LOCAL_NOW), LOCAL_NOW);
        assert_eq!(first.unwrap().change, LocalLayerChange::Effective);
        fs::write(dir.path().join(CONFIG_FILE_NAME), committed("'9GiB'")).unwrap();
        let landed = tracker
            .observe(inspect_local_config(dir.path(), LOCAL_NOW), LOCAL_NOW)
            .unwrap();
        assert_eq!(landed.change, LocalLayerChange::CommittedChanged);
        assert_eq!(landed.committed_changes.len(), 1, "{landed:?}");
        assert_eq!(landed.committed_changes[0].key, "run_size");
        assert!(
            tracker
                .observe(inspect_local_config(dir.path(), LOCAL_NOW), LOCAL_NOW)
                .is_none()
        );
    }

    /// The read keeps a value without its comment, however long, a `#` in
    /// a string kept (the parser's reading of the same line is the
    /// baseline), and a rejection's reason cut to [`REASON_CHARS`].
    #[test]
    fn the_read_keeps_values_without_comments_and_bounds_its_reason() {
        use crate::domain::local_config::REASON_CHARS;
        let dir = local_checkout();
        let comment = "#".repeat(1_100 * 1024);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[ci_watch]\nworkflow = 'CI'\nscheduled_workflows = ['a#b'] # committed\n[stall]\nidle_without_receipt_secs = 100 # c\n",
        )
        .unwrap();
        let local = dir.path().join(LOCAL_CONFIG_FILE_NAME);
        fs::write(
            &local,
            format!(
                "{LOCAL_VALID}[stall]\nidle_without_receipt_secs = 200 {comment}\n[ci_watch]\nscheduled_workflows = ['a#b', \"c#d\"] # x\n"
            ),
        )
        .unwrap();
        let read = inspect_local_config(dir.path(), LOCAL_NOW);
        assert_eq!(read.state, LocalLayerState::Effective, "{:?}", read.state);
        let key = |name: &str| read.keys.iter().find(|key| key.key == name).unwrap();
        assert_eq!(key("idle_without_receipt_secs").local_text, "200");
        assert_eq!(
            key("idle_without_receipt_secs").committed_text.as_deref(),
            Some("100")
        );
        assert_eq!(key("scheduled_workflows").local_text, "['a#b', \"c#d\"]");
        assert_eq!(
            key("scheduled_workflows").committed_text.as_deref(),
            Some("['a#b']")
        );
        let parsed = parse_config(&format!(
            "[ci_watch]\nworkflow = 'CI'\nscheduled_workflows = {}\n",
            key("scheduled_workflows").local_text
        ))
        .unwrap();
        assert_eq!(
            format!("{:?}", parsed.ci_watch),
            format!(
                "{:?}",
                parse_config(
                    "[ci_watch]\nworkflow = 'CI'\nscheduled_workflows = ['a#b', \"c#d\"] # x\n"
                )
                .unwrap()
                .ci_watch
            )
        );
        // A basic string's escaped quote does not end it.
        assert_eq!(value_without_comment(r##""c\"#d" # x"##), r##""c\"#d""##);
        fs::write(
            &local,
            format!(
                "{LOCAL_VALID}[{}]\nx = 1\n",
                "\u{1}".repeat(2 * 1024 * 1024)
            ),
        )
        .unwrap();
        let LocalLayerState::Rejected { reason } =
            inspect_local_config(dir.path(), LOCAL_NOW).state
        else {
            panic!("a disallowed table is rejected");
        };
        assert!(
            reason.starts_with("local table not allowed: "),
            "{reason:.80}"
        );
        assert_eq!(reason.chars().count(), REASON_CHARS + 1);
        assert!(reason.ends_with('…'));
    }

    /// A layer value long enough that its record passes
    /// [`crate::domain::local_config::MAX_RECORD_BYTES`] (an underscore
    /// spelling of 200 the parser takes) is cut in the record the
    /// supervisor fits, and `status` still shows the committed value that a
    /// landing then changes, from the record read back, as a supervisor's
    /// event and `status`' read of it carry it.
    #[test]
    fn a_fitted_record_of_a_long_value_still_shows_a_committed_change() {
        use crate::domain::local_config::{LocalLayerRecord, LocalLayerTracker, MAX_RECORD_BYTES};
        let dir = local_checkout();
        let base = dir.path().join(CONFIG_FILE_NAME);
        fs::write(&base, "[stall]\nidle_without_receipt_secs = 100\n").unwrap();
        let long = format!("{}200", "0_".repeat(50_000));
        let line = format!("[stall]\nidle_without_receipt_secs = {long}\n");
        assert_eq!(
            parse_config(&line).unwrap().stall.idle_without_receipt_secs,
            200
        );
        fs::write(
            dir.path().join(LOCAL_CONFIG_FILE_NAME),
            format!("{LOCAL_VALID}{line}"),
        )
        .unwrap();
        let mut tracker = LocalLayerTracker::default();
        let mut record = tracker
            .observe(inspect_local_config(dir.path(), LOCAL_NOW), LOCAL_NOW)
            .unwrap();
        let size = |record: &LocalLayerRecord| serde_json::to_vec(record).unwrap().len();
        assert!(size(&record) > MAX_RECORD_BYTES);
        record.fit();
        assert!(size(&record) <= MAX_RECORD_BYTES);
        let back = LocalLayerRecord::of(&serde_json::to_value(&record).unwrap()).unwrap();
        fs::write(&base, "[stall]\nidle_without_receipt_secs = 150\n").unwrap();
        let report = inspect_local_config(dir.path(), LOCAL_NOW)
            .report(Some(&back), LOCAL_NOW)
            .unwrap();
        assert_eq!(
            report["keys"][0]["committed_changed"],
            serde_json::json!({"from": "100", "to": "150"})
        );
        fs::write(&base, "[stall]\nidle_without_receipt_secs = 1_00\n").unwrap();
        let respelled = inspect_local_config(dir.path(), LOCAL_NOW)
            .report(Some(&back), LOCAL_NOW)
            .unwrap();
        assert_eq!(
            respelled["keys"][0]["committed_changed"],
            serde_json::Value::Null
        );
    }

    /// The rules applied and the layer recorded are of the same read: the
    /// stall value applied is the one the read's key gives, for each
    /// version of the file.
    #[test]
    fn the_operational_rules_and_the_layer_come_from_one_read() {
        struct At(i64);
        impl crate::application::Clock for At {
            fn system_time(&self) -> std::time::SystemTime {
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.0 as u64)
            }
            fn monotonic(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
        }
        let dir = local_checkout();
        for secs in [200, 300] {
            fs::write(
                dir.path().join(LOCAL_CONFIG_FILE_NAME),
                format!("{LOCAL_VALID}[stall]\nidle_without_receipt_secs = {secs}\n"),
            )
            .unwrap();
            let (rules, layer) = load_operational_snapshot(dir.path(), &At(LOCAL_NOW));
            assert_eq!(rules.unwrap().stall.idle_without_receipt_secs, secs);
            assert_eq!(layer.keys[0].local_text, secs.to_string());
        }
        let (rules, layer) = load_operational_snapshot(dir.path(), &At(LOCAL_NOW + 86_400_000));
        assert_eq!(layer.state, LocalLayerState::Expired);
        assert_ne!(rules.unwrap().stall.idle_without_receipt_secs, 300);
    }

    /// `doctor`'s check that the layer is ignored judges as Git does: the
    /// gitignore patterns (`**`, classes with ranges and `[:name:]`,
    /// escapes, anchoring, directories only, the case folding of
    /// `core.ignoreCase`), the last match of a source deciding, and the root
    /// `.gitignore` before `info/exclude` before `core.excludesFile`. Each
    /// expectation is what `git check-ignore` (2.54) answers for the same
    /// `.gitignore`, with `core.ignoreCase` false and true.
    #[test]
    fn local_layer_is_ignored_as_git_judges_it() {
        let dir = local_checkout();
        let home = tempfile::tempdir().unwrap();
        let ignored = || local_config_ignored_with(dir.path(), Some(home.path()), None).unwrap();
        assert!(!ignored());
        // (patterns, ignored, ignored under core.ignoreCase)
        for (patterns, expected, folded) in [
            ("dagq.local.toml\n", true, true),
            ("/dagq.local.toml\n", true, true),
            ("**/dagq.local.toml\n", true, true),
            ("*.local.toml\n", true, true),
            ("dagq.local.tom?\n", true, true),
            ("dagq.[kl]ocal.toml\n", true, true),
            ("dagq.[!l]ocal.toml\n", false, false),
            ("dagq.[^l]ocal.toml\n", false, false),
            ("dagq.[]l]ocal.toml\n", true, true),
            ("dagq.[!]x]ocal.toml\n", true, true),
            ("dagq.[k-m]ocal.toml\n", true, true),
            ("dagq.[K-M]ocal.toml\n", false, true),
            ("dagq.[L]ocal.toml\n", false, false),
            ("dagq.[\\k-m]ocal.toml\n", true, true),
            ("dagq.[a-]ocal.toml\n", false, false),
            ("dagq.[a-c-m]ocal.toml\n", false, false),
            ("dagq.[[:lower:]]ocal.toml\n", true, true),
            ("dagq.[[:upper:]]ocal.toml\n", false, true),
            ("dagq.[[:alpha:][:digit:]]ocal.toml\n", true, true),
            ("dagq[[:punct:]]local.toml\n", true, true),
            ("dagq.[[:punct:]]local.toml\n", false, false),
            ("dagq.[[:lower:]-z]ocal.toml\n", true, true),
            ("dagq.[[:lower]ocal.toml\n", true, true),
            ("dagq.[[:]ocal.toml\n", false, false),
            ("dagq.[[:bogus:]]ocal.toml\n", false, false),
            ("dagq.local.toml[\n", false, false),
            ("*.toml\n!dagq.[[:lower:]]ocal.toml\n", false, false),
            ("*\n!dagq.[[:bogus:]]ocal.toml\n", true, true),
            ("DAGQ.LOCAL.TOML\n", false, true),
            ("*.TOML\n", false, true),
            ("\\DAGQ.local.toml\n", false, false),
            ("\u{feff}dagq.local.toml\n", true, true),
            ("dagq.local.toml\r\n", true, true),
            ("dagq.local.toml\0junk\n", true, true),
            ("dagq.local.toml   \n", true, true),
            ("dagq.local.toml\\\\ \n", false, false),
            ("\\!dagq.local.toml\n", false, false),
            ("dagq.local.toml/\n", false, false),
            ("sub/dagq.local.toml\n", false, false),
            ("sub/**/dagq.local.toml\n", false, false),
            ("dagq.toml\n", false, false),
            ("dagq.local.toml\n!dagq.local.toml\n", false, false),
            ("!dagq.local.toml\n*.toml\n", true, true),
        ] {
            fs::write(dir.path().join(".gitignore"), patterns).unwrap();
            fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
            assert_eq!(ignored(), expected, "{patterns:?}");
            fs::write(
                dir.path().join(".git/config"),
                "[core]\nbare = false\n\tignorecase = true\n",
            )
            .unwrap();
            assert_eq!(ignored(), folded, "{patterns:?} under ignoreCase");
        }
        // The repository's ignoreCase is over the user's; a key without a
        // value is true.
        fs::write(dir.path().join(".gitignore"), "DAGQ.local.toml\n").unwrap();
        fs::write(home.path().join(".gitconfig"), "[core]\n\tignorecase\n").unwrap();
        fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        assert!(ignored());
        fs::write(
            dir.path().join(".git/config"),
            "[core]\n\tignoreCase = false\n",
        )
        .unwrap();
        assert!(!ignored());
        fs::remove_file(home.path().join(".gitconfig")).unwrap();
        fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        // A symlinked .gitignore is not followed, as Git does not.
        fs::write(home.path().join("linked"), "dagq.local.toml\n").unwrap();
        fs::remove_file(dir.path().join(".gitignore")).unwrap();
        std::os::unix::fs::symlink(home.path().join("linked"), dir.path().join(".gitignore"))
            .unwrap();
        assert!(!ignored());
        fs::remove_file(dir.path().join(".gitignore")).unwrap();
        // A match in .gitignore decides before info/exclude, either way.
        fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        fs::write(dir.path().join(".git/info/exclude"), "dagq.local.toml\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "!dagq.local.toml\n").unwrap();
        assert!(!ignored());
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        assert!(ignored());
        // core.excludesFile, of the user's config, then of the repository;
        // a key may follow its header, a comment ends a value, and a
        // relative path is of the root.
        fs::write(dir.path().join(".git/info/exclude"), "").unwrap();
        fs::write(home.path().join("global-ignore"), "dagq.local.toml\n").unwrap();
        fs::write(
            home.path().join(".gitconfig"),
            "[core] excludesFile = ~/global-ignore ; the user's\n",
        )
        .unwrap();
        assert!(ignored());
        fs::write(dir.path().join(".git/info/exclude"), "!dagq.local.toml\n").unwrap();
        assert!(!ignored());
        fs::write(dir.path().join(".git/info/exclude"), "").unwrap();
        fs::write(
            dir.path().join(".git/config"),
            "[core]\n\tbare = false\n\texcludesfile = \"/nonexistent/ignore\"\n",
        )
        .unwrap();
        assert!(!ignored());
        fs::write(dir.path().join("relative-ignore"), "dagq.local.toml\n").unwrap();
        fs::write(
            dir.path().join(".git/config"),
            "[core]\n\texcludesFile = relative-ignore\n",
        )
        .unwrap();
        assert!(ignored());
        // The default excludes file is under the XDG config directory.
        fs::write(dir.path().join(".git/config"), "[core]\nbare = false\n").unwrap();
        fs::remove_file(home.path().join(".gitconfig")).unwrap();
        fs::create_dir_all(home.path().join(".config/git")).unwrap();
        fs::write(home.path().join(".config/git/ignore"), "dagq.local.toml\n").unwrap();
        assert!(ignored());
    }

    /// The ignore check reads Git's configs as Git does: a BOM, a value's
    /// comment, quotes, whitespace and `\` line joins, `:(optional)`,
    /// booleans, `/dev/null` as no ignore file, `[include]` and
    /// `[includeIf]` with `gitdir:`, `gitdir/i:` and `onbranch:` (another
    /// condition never holds), and a config Git refuses or a `hasconfig:`
    /// include as the error. Each expectation is what `git check-ignore`
    /// (2.54) answers for the same files, an error where Git fails.
    #[test]
    fn the_ignore_check_reads_git_configs_as_git_does() {
        let dir = local_checkout();
        let home = tempfile::tempdir().unwrap();
        let ignored = || local_config_ignored_with(dir.path(), Some(home.path()), None);
        let user = home.path().join(".gitconfig");
        let ig = home.path().join("ig");
        fs::write(&ig, "dagq.local.toml\n").unwrap();
        fs::write(home.path().join("my\tig"), "dagq.local.toml\n").unwrap();
        fs::write(home.path().join("inc"), "[core]\n\texcludesFile = ~/ig\n").unwrap();
        fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let home_dir = home.path().display().to_string();
        let repo = dir.path().display().to_string();
        let name = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let include_if = |condition: &str| format!("[includeIf \"{condition}\"]\n\tpath = ~/inc\n");
        for (config, expected) in [
            (
                "[core]\n\texcludesFile = /dev/null\n".to_owned(),
                Some(false),
            ),
            (
                format!("[core]\n\texcludesFile = :(optional){home_dir}/ig\n"),
                Some(true),
            ),
            (
                format!("[core]\n\texcludesFile = {home_dir}/i\\\ng\n"),
                Some(true),
            ),
            (
                format!("[core]\n\texcludesFile = {home_dir}/my\tig\n"),
                Some(true),
            ),
            (
                format!("[core]\n\texcludesFile = {home_dir}/ig  ; c\n"),
                Some(true),
            ),
            (
                format!("[core]\n\texcludesFile = \"{home_dir}/ig\" # c\n"),
                Some(true),
            ),
            ("[include]\n\tpath = ~/inc\n".to_owned(), Some(true)),
            ("[include]\n\tpath = inc\n".to_owned(), Some(true)),
            (include_if(&format!("gitdir:{repo}/")), Some(true)),
            (include_if(&format!("gitdir:{name}/")), Some(true)),
            (include_if("gitdir:/nowhere/"), Some(false)),
            (
                include_if(&format!("gitdir/i:{}/", repo.to_uppercase())),
                Some(true),
            ),
            (
                include_if(&format!("gitdir:{}/", repo.to_uppercase())),
                Some(false),
            ),
            (include_if("onbranch:main"), Some(true)),
            (include_if("onbranch:dev"), Some(false)),
            (include_if("bogus:x"), Some(false)),
            (include_if("hasconfig:remote.*.url:https://x/**"), None),
            (format!("[core]\n\texcludesFile = \"{home_dir}/ig\n"), None),
            (format!("[core]\n\texcludesFile = {home_dir}/i\\qg\n"), None),
            ("[core]\n\texcludesFile\n".to_owned(), None),
            ("[other]\n\tx = \"unterminated\n".to_owned(), None),
        ] {
            fs::write(&user, &config).unwrap();
            assert_eq!(ignored().ok(), expected, "{config:?}: {:?}", ignored());
        }
        // ignoreCase of the user's config, for a .gitignore in another case.
        fs::write(dir.path().join(".gitignore"), "DAGQ.LOCAL.TOML\n").unwrap();
        for (config, expected) in [
            ("\u{feff}[core]\n\tignorecase = true\n", Some(true)),
            ("[core]\n\tignorecase = 1k\n", Some(true)),
            ("[core]\n\tignorecase = 0x0\n", Some(false)),
            ("[core]\n\tignorecase = \"tr\"ue\n", Some(true)),
            ("[core]\n\tignorecase = maybe\n", None),
        ] {
            fs::write(&user, config).unwrap();
            assert_eq!(ignored().ok(), expected, "{config:?}: {:?}", ignored());
        }
        // An include nested past Git's depth is the error.
        fs::write(&user, "[include]\n\tpath = ~/.gitconfig\n").unwrap();
        assert!(format!("{:#}", ignored().unwrap_err()).contains("deeper"));
    }

    /// A `.gitignore` line of any length is matched without recursion and
    /// in work bounded by its length: 100 000 `**/` before a name that
    /// does not match, a run of 100 000 stars, a 300 000-byte literal, and
    /// 100 000 `**/` before the file's name, which matches. Each answer is
    /// what `git check-ignore` (2.54) gives for the same line with 1 000
    /// repetitions.
    #[test]
    fn a_long_gitignore_line_is_matched_without_recursion() {
        let dir = local_checkout();
        let home = tempfile::tempdir().unwrap();
        let ignored = || local_config_ignored_with(dir.path(), Some(home.path()), None).unwrap();
        for (line, expected) in [
            (format!("{}never", "**/".repeat(100_000)), false),
            (format!("{}.toml", "*".repeat(100_000)), true),
            ("*a".repeat(100_000), false),
            ("x".repeat(300_000), false),
            (format!("{}dagq.local.toml", "**/".repeat(100_000)), true),
        ] {
            fs::write(dir.path().join(".gitignore"), format!("{line}\n")).unwrap();
            assert_eq!(ignored(), expected, "{:.12}…", line);
        }
    }

    /// The ignore check reads each of its files as `dagq.toml` is read: a
    /// `.gitignore` that is a FIFO, an `info/exclude` that is a symlink to
    /// `/dev/zero` or larger than [`MAX_CONFIG_BYTES`], a user's config
    /// that is a FIFO and a `.git` that is one are not read on but are the
    /// check's error with why, at once (the FIFO has no writer, so a read
    /// would block); a missing file is none.
    #[test]
    fn the_ignore_check_reads_its_files_bounded_and_says_why_it_cannot() {
        let dir = local_checkout();
        let home = tempfile::tempdir().unwrap();
        let ignored = || local_config_ignored_with(dir.path(), Some(home.path()), None);
        let fifo = |path: &Path| {
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        };
        let refused = |why: &str, file: &str| {
            let error = format!("{:#}", ignored().unwrap_err());
            assert!(error.contains(why) && error.contains(file), "{error}");
        };
        assert!(!ignored().unwrap());
        let gitignore = dir.path().join(".gitignore");
        fifo(&gitignore);
        refused("not a regular file", ".gitignore");
        fs::remove_file(&gitignore).unwrap();

        fs::create_dir_all(dir.path().join(".git/info")).unwrap();
        let exclude = dir.path().join(".git/info/exclude");
        std::os::unix::fs::symlink("/dev/zero", &exclude).unwrap();
        refused("not a regular file", "exclude");
        fs::remove_file(&exclude).unwrap();
        let large = vec![b'#'; usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1];
        fs::write(&exclude, &large).unwrap();
        refused("larger than the", "exclude");
        fs::remove_file(&exclude).unwrap();

        fifo(&home.path().join(".gitconfig"));
        refused("not a regular file", ".gitconfig");
        fs::remove_file(home.path().join(".gitconfig")).unwrap();

        let admin = dir.path().join("admin");
        fs::rename(dir.path().join(".git"), &admin).unwrap();
        fifo(&dir.path().join(".git"));
        refused("not a regular file", ".git");
        fs::remove_file(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git"), "gitdir: admin\n").unwrap();
        fs::write(admin.join("info/exclude"), "dagq.local.toml\n").unwrap();
        assert!(ignored().unwrap());
    }

    #[test]
    fn local_layer_is_not_read_from_a_linked_worktree_or_blob_text() {
        let dir = local_checkout();
        let admin = dir.path().join("admin");
        fs::rename(dir.path().join(".git"), &admin).unwrap();
        fs::write(
            dir.path().join(".git"),
            format!("gitdir: {}", admin.display()),
        )
        .unwrap();
        fs::write(admin.join("commondir"), "../main/.git").unwrap();
        fs::write(
            dir.path().join(LOCAL_CONFIG_FILE_NAME),
            format!("{LOCAL_VALID}[disk]\nreserve = '12%'"),
        )
        .unwrap();
        let text = read_config_at(&dir.path().join(CONFIG_FILE_NAME), LOCAL_NOW)
            .unwrap()
            .unwrap();
        assert_eq!(parse_config(&text).unwrap().disk.reserve, 0.05);
        // Committed-tree routes parse the blob directly, without a local read.
        assert_eq!(
            parse_config("[disk]\nreserve = '5%'").unwrap().disk.reserve,
            0.05
        );
    }

    /// A `dagq.toml` is read only as a regular file of at most
    /// [`MAX_CONFIG_BYTES`]: a symlink to `/dev/zero` or to a FIFO, a
    /// directory and a larger file are refused with why, not read on;
    /// a symlink to a regular file and a missing file read as before.
    #[test]
    fn a_dagq_toml_is_read_only_as_a_bounded_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE_NAME);
        let refused = |why: &str| {
            let error = format!("{:#}", load_kpi_settings(dir.path()).unwrap_err());
            assert!(error.contains(why), "{error}");
        };
        assert_eq!(read_config(&file).unwrap(), None);

        std::os::unix::fs::symlink("/dev/zero", &file).unwrap();
        refused("not a regular file");
        std::fs::remove_file(&file).unwrap();

        let fifo = dir.path().join("fifo");
        let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink(&fifo, &file).unwrap();
        refused("not a regular file");
        std::fs::remove_file(&file).unwrap();

        std::fs::create_dir(&file).unwrap();
        refused("not a regular file");
        std::fs::remove_dir(&file).unwrap();

        let large = vec![b'#'; usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1];
        std::fs::write(&file, &large).unwrap();
        refused("larger than the");

        let real = dir.path().join("real.toml");
        std::fs::write(&real, "[kpi]\nmax_improvement_proposals = 3\n").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        assert_eq!(
            load_kpi_settings(dir.path())
                .unwrap()
                .and_then(|kpi| kpi.max_improvement_proposals),
            Some(3)
        );
    }

    /// The inbox reads its own table alone: a mistake there is an error
    /// whatever its message, one in another table is not seen.
    #[test]
    fn the_inbox_table_is_read_alone_and_its_mistakes_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(CONFIG_FILE_NAME);
        let load = |text: &str| {
            std::fs::write(&file, text).unwrap();
            load_inbox_role(dir.path()).map_err(|error| format!("{error:#}"))
        };
        assert_eq!(load_inbox_role(dir.path()).unwrap(), RoleModels::default());
        let other = "[no_such_table]\nkey = 1\n[roles.review]\nprovider = 'gemini'\n";
        assert!(parse_config(other).is_err());
        assert_eq!(load(other).unwrap(), RoleModels::default());
        let codex = load(
            "[no_such_table]\nkey = 1\n[roles.inbox] # the inbox\nprovider = 'codex'\n[x]\ny = 2\n",
        )
        .unwrap();
        assert_eq!(
            codex.get(ModelRole::Inbox).unwrap().provider,
            Some(crate::domain::Provider::Codex)
        );
        for (text, line) in [
            ("[a]\nb = 1\n[roles.inbox]\nprovider = \"gemini\"\n", ":4"),
            (
                "[roles.inbox]\nprovider = 'codex'\nmodel = 'claude-opus-5-5'\n",
                "[roles.inbox]",
            ),
            ("[roles.inbox]\nflavour = 'x'\n", ":2"),
        ] {
            let error = load(text).unwrap_err();
            assert!(error.contains(line), "{text:?}: {error}");
        }
        assert!(
            load("[roles.inbox]\nprovider = \"gemini\"\n")
                .unwrap_err()
                .contains("gemini")
        );
    }

    use super::*;

    /// This repository's own `dagq.toml` declares the Rust profile and has
    /// the project rules the work breakdown's tests classify by
    /// ([`crate::domain::command_rules::project_rules`]), so those tests
    /// hold for the file itself.
    #[test]
    fn this_repositorys_dagq_toml_has_the_rules_the_tests_classify_by() {
        let config = parse_config(include_str!("../../dagq.toml")).unwrap();
        assert!(
            config
                .toolchains
                .iter()
                .any(|toolchain| toolchain.profile == "rust"),
            "{:?}",
            config.toolchains
        );
        let mut commands = config.telemetry.commands;
        for rule in &mut commands {
            rule.line = None;
        }
        assert_eq!(commands, crate::domain::command_rules::project_rules());
        assert!(!config.telemetry.failed_tests);
    }

    /// This repository's `dagq.toml` prices Codex and gives it a first
    /// run's estimate, so an eval round on Codex is estimated before any
    /// run of the agent on Codex was measured instead of refused as
    /// cost unknown.
    #[test]
    fn this_repositorys_dagq_toml_estimates_a_codex_round_without_a_measured_run() {
        use crate::domain::Provider;
        use crate::domain::agent_eval::round::{EstimateSource, estimate};
        let config = parse_config(include_str!("../../dagq.toml")).unwrap().eval;
        let prices = config.provider(Provider::Codex);
        assert!(prices.converts(), "{prices:?}");
        assert!(prices.cached_input_usd_per_mtok.is_some(), "{prices:?}");
        let one = estimate(1, Provider::Codex, &[], &config).unwrap();
        assert_eq!(one.per_run_source, EstimateSource::ProviderDefault);
        assert_eq!(Some(one.per_run_usd), prices.default_run_usd);
    }

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

    /// `[local_checks]` and `[local_checks.gates.<name>]` declare the
    /// tests and the gates, a gate needs the tests, and a mistake in one
    /// is refused with its line.
    #[test]
    fn parses_the_local_gates_tables() {
        use crate::domain::local_gates::{LocalGate, LocalGates};
        let config = parse_config(
            "[local_checks]\ntests = ['src/**', \"tests/**\", 'src/**']\n[local_checks.gates.stress]\npaths = ['src/**', 'tests/**']\nkind = 'stress'\n[local_checks.gates.\"it-time\"]\nkind = \"it time\" # its records\npaths = ['tests/**']\n[run.env]\nA = '1'\n",
        )
        .unwrap();
        assert_eq!(
            config.local_gates,
            Some(LocalGates {
                tests: vec!["src/**".into(), "tests/**".into()],
                gates: vec![
                    LocalGate {
                        name: "stress".into(),
                        paths: vec!["src/**".into(), "tests/**".into()],
                        kind: "stress".into(),
                    },
                    LocalGate {
                        name: "it-time".into(),
                        paths: vec!["tests/**".into()],
                        kind: "it time".into(),
                    },
                ],
            })
        );
        // No gate: nothing is judged, the tests' globs alone too.
        assert_eq!(parse_config("").unwrap().local_gates, None);
        assert_eq!(
            parse_config("[local_checks]\ntests = ['src/**']\n")
                .unwrap()
                .local_gates,
            None
        );
        for (text, error) in [
            (
                "[local_checks.gates.a]\npaths = ['x/**']\nkind = 'a'\n",
                "[local_checks.gates.a] needs [local_checks] tests",
            ),
            (
                "[local_checks.gates]\n",
                "dagq.toml:1: [local_checks.gates] names no gate",
            ),
            ("[local_checks.gates.A]\n", "is not kebab-case"),
            (
                "[local_checks]\ntests = ['a']\n[local_checks.gates.a]\nkind = 'a'\n",
                "dagq.toml:3: [local_checks.gates.a] has no paths",
            ),
            (
                "[local_checks]\ntests = ['a']\n[local_checks.gates.a]\npaths = ['a']\n[run.env]\n",
                "dagq.toml:3: [local_checks.gates.a] has no kind",
            ),
            (
                "[local_checks.gates.a]\nkind = ' '\n",
                "dagq.toml:2: kind of [local_checks.gates.a] is blank",
            ),
            (
                "[local_checks.gates.a]\npaths = []\n",
                "dagq.toml:2: paths names no glob",
            ),
            (
                "[local_checks.gates.a]\npaths = ['/a']\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[local_checks.gates.a]\nkind = 'a'\nkind = 'b'\n",
                "dagq.toml:3: kind is defined twice",
            ),
            (
                "[local_checks.gates.a]\nname = 'a'\n",
                "unknown key name in [local_checks.gates.a]",
            ),
            (
                "[local_checks.gates.a]\nkind = 'a'\npaths = ['a']\n[local_checks.gates.a]\n",
                "dagq.toml:4: [local_checks.gates.a] is defined twice",
            ),
            (
                "[local_checks]\ntests = ['a']\ntests = ['b']\n",
                "dagq.toml:3: tests is defined twice",
            ),
            (
                "[local_checks]\npaths = ['a']\n",
                "unknown key paths in [local_checks]",
            ),
            ("[local_checks]\ntests = []\n", "tests names no glob"),
            (
                "[local_checks]\n[local_checks]\n",
                "[local_checks] is defined twice",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text:?}: {message}");
        }
        // The review's reading resolves the profiles beside the gates.
        let read = gate_config("[toolchains.web]\nprofile = 'typescript'\n");
        assert!(format!("{:#}", read.unwrap_err()).contains("unknown profile"));
        let read = gate_config_in(
            "[toolchains.made]\nprofile = 'fake'\n",
            &crate::domain::toolchain::fake::REGISTRY,
        )
        .unwrap();
        assert_eq!(read.gates, None);
        assert_eq!(read.toolchains.view()["declared"][0]["profile"], "fake");
    }

    /// The container's settings the profiles recommend are written in the
    /// tables and keys the container runtime reads: the proposal, once
    /// written, reads as the image, the registries and the worker cache it
    /// names.
    #[test]
    fn the_profiles_container_proposals_read_as_the_container_settings() {
        use crate::domain::toolchain::{BUILTIN, recommend::propose, rust::RECOMMENDATION};
        let view = propose(&BUILTIN, Ok(Vec::new()), || {
            Ok(vec!["Cargo.toml".to_owned()])
        })
        .view();
        let config = parse_config(view["config"].as_str().unwrap()).unwrap();
        let rust = RECOMMENDATION;
        assert_eq!(
            config.container_image,
            Some(ProjectImage {
                containerfile: crate::domain::toolchain::recommend::CONTAINERFILE.to_owned(),
                inputs: rust
                    .image
                    .unwrap()
                    .inputs
                    .iter()
                    .map(|i| (*i).to_owned())
                    .collect(),
            })
        );
        assert_eq!(
            config
                .egress_registries
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            rust.registries.iter().map(|r| r.host).collect::<Vec<_>>()
        );
        assert_eq!(config.container_worker_cache, rust.worker_cache);
    }

    /// `[toolchains.<name>]` declares a profile and the paths it applies
    /// to, one table per declaration in file order; a mistake in one is
    /// refused with its line, and a profile the registry does not have
    /// when the declarations are resolved.
    #[test]
    fn parses_the_toolchains_tables() {
        let config = parse_config(
            "[toolchains.core]\nprofile = \"rust\"\npaths = [\"src/**\", \"Cargo.toml\", \"src/**\"]\n[toolchains.\"web-ui\"]\nprofile = 'typescript' # not built in\n[run.env]\nA = '1'\n",
        )
        .unwrap();
        assert_eq!(
            config.toolchains,
            [
                ToolchainDeclaration {
                    name: "core".into(),
                    profile: "rust".into(),
                    paths: vec!["src/**".into(), "Cargo.toml".into()],
                    line: 1,
                },
                ToolchainDeclaration {
                    name: "web-ui".into(),
                    profile: "typescript".into(),
                    paths: Vec::new(),
                    line: 4,
                },
            ]
        );
        assert!(parse_config("").unwrap().toolchains.is_empty());
        for (text, error) in [
            (
                "[toolchains]\nprofile = 'rust'\n",
                "dagq.toml:1: [toolchains] names no toolchain",
            ),
            ("[toolchains.Core]\nprofile = 'rust'\n", "is not kebab-case"),
            (
                "[toolchains.a]\nprofile = 'rust'\n[toolchains.a]\nprofile = 'rust'\n",
                "dagq.toml:3: [toolchains.a] is defined twice",
            ),
            (
                "[toolchains.a]\npaths = ['src/**']\n",
                "dagq.toml:1: [toolchains.a] has no profile",
            ),
            (
                "[toolchains.a]\n[run.env]\nA = '1'\n",
                "dagq.toml:1: [toolchains.a] has no profile",
            ),
            (
                "[toolchains.a]\nprofile = ' '\n",
                "dagq.toml:2: profile of [toolchains.a] is blank",
            ),
            (
                "[toolchains.a]\nprofile = 'rust'\nprofile = 'rust'\n",
                "dagq.toml:3: profile is defined twice",
            ),
            (
                "[toolchains.a]\nprofile = 'rust'\nlanguage = 'rust'\n",
                "unknown key language in [toolchains.a]",
            ),
            (
                "[toolchains.a]\nprofile = 'rust'\npaths = []\n",
                "paths of [toolchains.a] names no glob",
            ),
            (
                "[toolchains.a]\nprofile = 'rust'\npaths = ['/abs/**']\n",
                "dagq.toml:3: value of paths",
            ),
            (
                "[toolchains.a]\nprofile = 'rust'\npaths = ['a/../b']\n",
                "dagq.toml:3: value of paths",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text:?}: {message}");
        }
        // The profile's name is checked against the registry once the
        // declarations are resolved.
        let dir = tempfile::tempdir().unwrap();
        let registry = &crate::domain::toolchain::BUILTIN;
        assert_eq!(
            load_toolchains(dir.path(), registry).unwrap().view(),
            serde_json::json!({"declared": [], "marks": []})
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[toolchains.web]\nprofile = 'typescript'\n",
        )
        .unwrap();
        assert_eq!(
            format!("{:#}", load_toolchains(dir.path(), registry).err().unwrap()),
            "dagq.toml:1: [toolchains.web]: unknown profile \"typescript\"; the profiles are rust"
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[toolchains.core]\nprofile = 'rust'\npaths = ['src/**']\n",
        )
        .unwrap();
        let toolchains = ShellVerifier {
            checkout: dir.path().to_path_buf(),
            db: dir.path().join("q.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: registry,
            children: crate::domain::environment::RunChildren::default(),
        }
        .toolchains()
        .unwrap();
        assert_eq!(
            toolchains.view()["declared"],
            serde_json::json!([{"name": "core", "profile": "rust", "paths": ["src/**"]}])
        );
    }

    /// `[verify_marks.<name>]` declares a class the runtime has and the
    /// shape of the line; a class it does not have, a malformed shape or a
    /// missing key is refused with its line, and the marks reach the
    /// classification through the toolchains read with them.
    #[test]
    fn parses_the_verify_marks_tables() {
        let config = parse_config(
            "[verify_marks.within]\nclass = \"timeout\"\nline = \"test {test} timed out: *did not happen within*\" # tests/common\n[verify_marks.\"bad-build\"]\nline = 'BAD BUILD*'\nclass = 'build_error'\n[run.env]\nA = '1'\n",
        )
        .unwrap();
        let shown: Vec<_> = config
            .verify_marks
            .iter()
            .map(|mark| (mark.name.as_str(), mark.class.as_str(), mark.line.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "within",
                    "timeout",
                    "test {test} timed out: *did not happen within*"
                ),
                ("bad-build", "build_error", "BAD BUILD*"),
            ]
        );
        assert!(parse_config("").unwrap().verify_marks.is_empty());
        for (text, error) in [
            (
                "[verify_marks]\nclass = 'timeout'\n",
                "dagq.toml:1: [verify_marks] names no mark",
            ),
            (
                "[verify_marks.Within]\nclass = 'timeout'\n",
                "is not kebab-case",
            ),
            (
                "[verify_marks.a]\nclass = 'timeout'\nline = 'x'\n[verify_marks.a]\n",
                "dagq.toml:4: [verify_marks.a] is defined twice",
            ),
            (
                "[verify_marks.a]\nline = 'x'\n",
                "dagq.toml:1: [verify_marks.a] has no class",
            ),
            (
                "[verify_marks.a]\nclass = 'timeout'\n[run.env]\nA = '1'\n",
                "dagq.toml:1: [verify_marks.a] has no line",
            ),
            (
                "[verify_marks.a]\nclass = 'slow'\n",
                "dagq.toml:2: value of class: unknown class \"slow\"; a mark names one of killed, timeout, build_error, lint, test_failure, format, coverage_below",
            ),
            (
                "[verify_marks.a]\nclass = 'disk_full'\n",
                "unknown class \"disk_full\"",
            ),
            (
                "[verify_marks.a]\nclass = 'flaky'\n",
                "unknown class \"flaky\"",
            ),
            (
                "[verify_marks.a]\nline = '{test}*'\n",
                "dagq.toml:2: value of line",
            ),
            (
                "[verify_marks.a]\nline = 'a {test} b {test}'\n",
                "dagq.toml:2: value of line",
            ),
            (
                "[verify_marks.a]\nclass = 'lint'\nclass = 'lint'\n",
                "dagq.toml:3: class is defined twice",
            ),
            (
                "[verify_marks.a]\nclass = 'lint'\npattern = 'x'\n",
                "unknown key pattern in [verify_marks.a]; the keys are class and line",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text:?}: {message}");
        }
        let dir = tempfile::tempdir().unwrap();
        let registry = &crate::domain::toolchain::BUILTIN;
        let within = "test a::b timed out: x did not happen within 1s\n";
        let class = |toolchains: &Toolchains| {
            crate::domain::verify_failure::classify(toolchains, "check", Some(101), None, within)
                .class
                .as_str()
        };
        // Neither declared: nothing reads the line.
        assert_eq!(
            class(&load_toolchains(dir.path(), registry).unwrap()),
            "unknown"
        );
        // A profile declared without the mark: the line is not read.
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[toolchains.core]\nprofile = 'rust'\n",
        )
        .unwrap();
        assert_eq!(
            class(&load_toolchains(dir.path(), registry).unwrap()),
            "unknown"
        );
        // The mark declared, with or without a profile.
        for toml in ["[toolchains.core]\nprofile = 'rust'\n", ""] {
            fs::write(
                dir.path().join(CONFIG_FILE_NAME),
                format!("{toml}[verify_marks.within]\nclass = 'timeout'\nline = 'test {{test}} timed out: *did not happen within*'\n"),
            )
            .unwrap();
            let toolchains = load_toolchains(dir.path(), registry).unwrap();
            assert_eq!(class(&toolchains), "timeout", "{toml:?}");
            assert_eq!(
                toolchains.view()["marks"],
                serde_json::json!([{"name": "within", "class": "timeout", "line": "test {test} timed out: *did not happen within*"}])
            );
        }
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

    /// `[review.subagents.<agent>] provider` names the provider of the
    /// agent's job, before or after its paths; an agent without it is read
    /// as before (`None`, where `[roles.review]` sends the review), and a
    /// provider this binary does not know is refused with its line, so the
    /// review that reads it does not pass (ADR-t1453-1 decision 4).
    #[test]
    fn an_agent_may_name_the_provider_of_its_job() {
        let config = parse_config(
            "[review.subagents.a]\nprovider = \"codex\" # its own\npaths = [\"src/**\"]\n\
             [review.subagents.b]\npaths = [\"docs/**\"]\nprovider = 'claude'\n\
             [review.subagents.c]\npaths = [\"tests/**\"]\n",
        )
        .unwrap();
        let providers: Vec<_> = config
            .review_subagents
            .iter()
            .map(|agent| (agent.name.as_str(), agent.provider))
            .collect();
        assert_eq!(
            providers,
            [
                ("a", Some(crate::domain::Provider::Codex)),
                ("b", Some(crate::domain::Provider::Claude)),
                ("c", None),
            ]
        );
        for (text, expected) in [
            (
                "[review.subagents.a]\npaths = [\"a\"]\nprovider = \"gemini\"\n",
                "dagq.toml:3: value of provider: ",
            ),
            (
                "[review.subagents.a]\npaths = [\"a\"]\nprovider = codex\n",
                "dagq.toml:3: value of provider",
            ),
            (
                "[review.subagents.a]\nprovider = \"codex\"\nprovider = \"claude\"\npaths = [\"a\"]\n",
                "dagq.toml:3: provider is defined twice",
            ),
            (
                "[review.subagents.a]\nprovider = \"codex\"\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
            (
                "[review.subagents.a]\nprovider = \"codex\"\n[run.env]\n",
                "dagq.toml:1: [review.subagents.a] has no paths",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let unknown = format!(
            "{:#}",
            parse_config("[review.subagents.a]\npaths = [\"a\"]\nprovider = \"gemini\"\n")
                .unwrap_err()
        );
        assert!(unknown.contains("gemini"), "{unknown}");
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
                    provider: None,
                    model: None,
                    effort: None,
                },
                ReviewSubagent {
                    name: "migrations".into(),
                    paths: vec!["migrations/*.sql".into()],
                    provider: None,
                    model: None,
                    effort: None,
                },
            ]
        );
        assert_eq!(config.run_env, [("A".to_owned(), "1".to_owned())]);
        assert!(parse_config("").unwrap().review_subagents.is_empty());
        for (text, expected) in [
            (
                "[review.subagents.a]\nglobs = [\"a\"]\n",
                "dagq.toml:2: unknown key globs in [review.subagents.a]; the keys are paths, provider, model and effort",
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
            ("[reviews]\n", "dagq.toml:1: unknown table [reviews]"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let verifier = ShellVerifier {
            checkout: PathBuf::from("/nonexistent"),
            db: PathBuf::from("/nonexistent/queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier
                .review_subagents_in("[review.subagents.a]\npaths = [\"x\"]\n")
                .unwrap()
                .agents
                .len(),
            1
        );
        assert!(verifier.review_subagents_in("[nope]\n").is_err());
    }

    /// `model` and `effort` of a project's agent and of a built-in are
    /// read as written, each optional and a string that is not blank;
    /// whether the provider the job starts on takes them is left to the
    /// review's reading ([`ReviewAgents::launch_problems`]), which checks
    /// them against the same file's `[roles.review]`.
    /// What `agent eval` checks a requested launch against: the roles and
    /// the agent's table of the checkout's `dagq.toml`, none without it.
    #[test]
    fn the_eval_launch_settings_are_the_roles_and_the_agents_table() {
        let dir = tempfile::tempdir().unwrap();
        let none = load_eval_launch_settings(dir.path(), "design").unwrap();
        assert_eq!(
            (none.roles, none.table),
            (
                RoleModels::default(),
                crate::domain::actor_model::AgentModel::default()
            )
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[roles.review]\nprovider = \"codex\"\n\
             [review.subagents.design]\npaths = [\"docs/**\"]\nprovider = \"claude\"\n\
             model = \"claude-haiku-5-5\"\n",
        )
        .unwrap();
        let read = load_eval_launch_settings(dir.path(), "design").unwrap();
        assert_eq!(
            read.roles.provider(ModelRole::Review).0,
            crate::domain::Provider::Codex
        );
        assert_eq!(read.table.provider, Some(crate::domain::Provider::Claude));
        assert_eq!(read.table.model.as_deref(), Some("claude-haiku-5-5"));
    }

    /// A request (`agent eval`) and a round's start read the same
    /// `[eval.providers.*]` of the checkout's `dagq.toml`, by the heading
    /// whatever its keys, so they decide a requested provider alike; the
    /// table changes neither the estimate of another provider nor where a
    /// review's own job of the agent starts.
    #[test]
    fn eval_providers_declare_a_rounds_sending_and_leave_the_review_alone() {
        use crate::domain::Provider;
        use crate::domain::actor_model::AgentModel;
        use crate::domain::agent_eval::round::check_requested_launch;
        let base = "[roles.review]\nprovider = \"codex\"\n\
                    [review.subagents.design]\npaths = [\"docs/**\"]\n";
        let dir = tempfile::tempdir().unwrap();
        let verifier = ShellVerifier {
            checkout: dir.path().to_path_buf(),
            db: dir.path().join("q.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        let asked = AgentModel {
            provider: Some(Provider::Claude),
            model: Some("claude-haiku-5-5".into()),
            effort: Some("low".into()),
        };
        // `decisions` is what a request and a round's start each decide.
        let decisions = |text: &str| {
            fs::write(dir.path().join(CONFIG_FILE_NAME), text).unwrap();
            let settings = load_eval_launch_settings(dir.path(), "design").unwrap();
            let at_start = verifier.eval_config().unwrap().declared_providers;
            assert_eq!(settings.declared, at_start);
            check_requested_launch(&asked, &settings.roles, &settings.table, &at_start)
        };
        let without = decisions(base);
        assert!(without.is_err());
        for table in [
            "[eval.providers.claude]\n",
            "[eval.providers.claude]\ndefault_run_usd = 0.2\n",
        ] {
            assert_eq!(decisions(&format!("{base}{table}")), Ok(asked.clone()));
        }
        let read = load_eval_config(dir.path()).unwrap().unwrap();
        assert_eq!(read.provider(Provider::Claude).default_run_usd, Some(0.2));
        // `[eval.providers.codex]` declares Codex, not Claude.
        assert!(decisions(&format!("{base}[eval.providers.codex]\n")).is_err());
        // A review's own job starts as without the table.
        let plan = |text: &str| {
            let config = parse_config(text).unwrap();
            config
                .roles
                .review_plan(&config.review_agents().written_launch("design"), None)
                .unwrap()
        };
        let declared = plan(&format!("{base}[eval.providers.claude]\n"));
        assert_eq!(declared, plan(base));
        assert_eq!(declared.launch.provider, Provider::Codex);
    }

    #[test]
    fn parses_the_review_agents_model_and_effort() {
        let config = parse_config(
            "[roles.review]\nprovider = \"codex\"\n\
             [review.subagents.design]\npaths = [\"docs/**\"]\nprovider = \"claude\"\n\
             model = \"claude-haiku-5-5\"\neffort = \"low\"\n\
             [review.subagents.\"builtin:security\"]\neffort = \"xhigh\"\n\
             [review.subagents.\"builtin:tests\"]\nmodel = \"claude-haiku-5-5\"\n",
        )
        .unwrap();
        let agents = config.review_agents();
        assert_eq!(
            agents.written_launch("design"),
            crate::domain::actor_model::AgentModel {
                provider: Some(crate::domain::Provider::Claude),
                model: Some("claude-haiku-5-5".into()),
                effort: Some("low".into()),
            }
        );
        assert_eq!(
            agents.written_launch("builtin:security").effort.as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            agents.written_launch("other"),
            crate::domain::actor_model::AgentModel::default()
        );
        // The built-in's model is Claude's on the role's Codex.
        assert_eq!(
            agents.launch_problems(),
            [
                "[review.subagents.\"builtin:tests\"]: model claude-haiku-5-5 is Claude's; with provider codex name a model of Codex's, or none for Codex's default"
            ]
        );
        for (text, expected) in [
            (
                "[review.subagents.a]\npaths = [\"x\"]\nmodel = \" \"\n",
                "dagq.toml:3: model is blank",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\neffort = 1\n",
                "dagq.toml:2: value of effort",
            ),
            (
                "[review.subagents.a]\npaths = [\"x\"]\nmodels = \"m\"\n",
                "the keys are paths, provider, model and effort",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    #[test]
    fn acceptance_precheck_is_optional_dedicated_and_never_selected_in_parallel() {
        use crate::domain::Provider;
        let off = parse_config("").unwrap().review_agents();
        assert!(!off.precheck.enabled);
        let parsed = parse_config("[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\nprovider = \"codex\"\nmodel = \"gpt-5.5\"\neffort = \"low\"\n").unwrap();
        let agents = parsed.review_agents();
        assert!(agents.precheck.enabled);
        assert_eq!(agents.precheck.launch().unwrap().provider, Provider::Codex);
        assert!(
            agents
                .selectable()
                .iter()
                .all(|a| a.name != review_builtins::ACCEPTANCE_PRECHECK)
        );
        for key in ["paths = [\"src/**\"]", "rules = \"x\""] {
            assert!(
                parse_config(&format!(
                    "[review.subagents.\"builtin:acceptance-precheck\"]\n{key}\n"
                ))
                .is_err()
            );
        }
        assert!(parse_config("[review.subagents.\"builtin:acceptance-precheck\"]\nenabled = true\nenabled = false\n").is_err());
    }

    /// `[review.subagents."builtin:<name>"]` sets a built-in agent's
    /// `enabled`, `paths` and `provider`, each optional, beside the
    /// project's agents (ADR-t2259-1 decision 4); `builtin:acceptance`
    /// may only say `enabled = true` (decision 3). A name of the prefix
    /// that is no built-in's (`builtin:docs` among them, ADR-t2389-1
    /// decision 1), which a project's agent cannot take, and an `enabled`
    /// that is no boolean are mistakes of the whole file, so a review that
    /// reads it does not pass.
    #[test]
    fn parses_the_builtin_review_agents() {
        let config = parse_config(
            "[review.subagents.\"builtin:tests\"]\nenabled = true # on\n\
             paths = [\"lib/**\", \"lib/**\"]\nprovider = \"codex\"\n\
             [review.subagents.design]\npaths = [\"docs/**\"]\n\
             [review.subagents.'builtin:security']\nenabled = false\n\
             [review.subagents.\"builtin:acceptance\"]\nenabled = true\n",
        )
        .unwrap();
        assert_eq!(
            config.review_builtins,
            [
                BuiltinSetting {
                    agent: BuiltinAgent::Tests,
                    enabled: Some(true),
                    paths: Some(vec!["lib/**".into()]),
                    provider: Some(crate::domain::Provider::Codex),
                    model: None,
                    effort: None,
                },
                BuiltinSetting {
                    agent: BuiltinAgent::Security,
                    enabled: Some(false),
                    paths: None,
                    provider: None,
                    model: None,
                    effort: None,
                },
            ]
        );
        assert_eq!(
            config
                .review_agents()
                .selectable()
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>(),
            ["design", "builtin:tests"]
        );
        // A table of only its header leaves the built-in to its defaults.
        let header =
            parse_config("[review.subagents.\"builtin:tests\"]\n[run.env]\nA = \"1\"\n").unwrap();
        assert_eq!(header.review_builtins[0].enabled, None);
        assert!(!parse_config("").unwrap().review_agents().written());
        // The whole review's table with `enabled = true` alone, or with
        // nothing, reads as no table: the review goes as without one.
        for whole in [
            "[review.subagents.\"builtin:acceptance\"]\nenabled = true\n",
            "[review.subagents.\"builtin:acceptance\"]\n",
        ] {
            assert_eq!(
                parse_config(whole).unwrap().review_agents(),
                parse_config("").unwrap().review_agents(),
                "{whole}"
            );
        }
        for (text, expected) in [
            (
                "[review.subagents.\"builtin:docs\"]\nenabled = true\n",
                "dagq.toml:1: [review.subagents.\"builtin:docs\"]: builtin:docs is no built-in agent",
            ),
            (
                "[review.subagents.\"builtin:my-checks\"]\npaths = [\"a\"]\n",
                "names starting with builtin: are the built-ins', so a project's agent may not take one",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\nenabled = \"yes\"\n",
                "dagq.toml:2: value of enabled: expected true or false",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\nenabled = true\nenabled = false\n",
                "dagq.toml:3: enabled is defined twice",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\npaths = []\n",
                "dagq.toml:2: paths of [review.subagents.\"builtin:tests\"] names no glob",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\nscript = \"a.sh\"\n",
                "dagq.toml:2: unknown key script in [review.subagents.\"builtin:tests\"]; the keys are enabled, paths, provider, model and effort",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\nprovider = \"gemini\"\n",
                "dagq.toml:2: value of provider",
            ),
            (
                "[review.subagents.\"builtin:tests\"]\n[review.subagents.\"builtin:tests\"]\n",
                "dagq.toml:2: [review.subagents.\"builtin:tests\"] is defined twice",
            ),
            (
                "[review.subagents.\"builtin:acceptance\"]\nenabled = false\n",
                "dagq.toml:2: builtin:acceptance cannot be turned off",
            ),
            (
                "[review.subagents.\"builtin:acceptance\"]\npaths = [\"a\"]\n",
                "dagq.toml:2: unknown key paths in [review.subagents.\"builtin:acceptance\"]",
            ),
            (
                "[review.subagents.\"builtin:acceptance\"]\nprovider = \"codex\"\n",
                "dagq.toml:2: unknown key provider in [review.subagents.\"builtin:acceptance\"]",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    /// `[review.programs.<name>]` names a program review: a repository
    /// script with its arguments, the globs that require it and an
    /// optional time limit (ADR-t1895-2). A mistake in it is an error
    /// naming the line; `command`, which would name a program the worker's
    /// worktree can supply, is one that says to write a script; a file
    /// without the table, as every older one, reads as before with no
    /// program.
    #[test]
    fn parses_the_review_programs() {
        let config = parse_config(
            "[review.programs.fmt]\nscript = \"scripts/fmt.sh\" # fast\npaths = [\"src/**\", \"src/**\"]\n\
             [review.programs.\"links\"]\nscript = \"scripts/check-links.sh\"\nargs = [\"--quiet\"]\npaths = ['docs/**']\ntimeout_secs = 30\n\
             [run.env]\nA = \"1\"\n",
        )
        .unwrap();
        assert_eq!(
            config.review_programs,
            [
                ReviewProgram {
                    name: "fmt".to_owned(),
                    script: "scripts/fmt.sh".to_owned(),
                    args: Vec::new(),
                    paths: vec!["src/**".to_owned()],
                    timeout_secs: None,
                    builtin: None,
                },
                ReviewProgram {
                    name: "links".to_owned(),
                    script: "scripts/check-links.sh".to_owned(),
                    args: vec!["--quiet".to_owned()],
                    paths: vec!["docs/**".to_owned()],
                    timeout_secs: Some(30),
                    builtin: None,
                },
            ]
        );
        assert_eq!(config.run_env, pairs(&[("A", "1")]));
        for text in [
            "",
            "[run.env]\nA = \"1\"\n[review.subagents.a]\npaths = [\"x\"]\n",
        ] {
            assert!(parse_config(text).unwrap().review_programs.is_empty());
        }
        for (text, error) in [
            (
                "[review.programs]\n",
                "dagq.toml:1: [review.programs] names no program",
            ),
            (
                "[review.programs.Fmt]\n",
                "dagq.toml:1: program \"Fmt\" of [review.programs.Fmt] is not kebab-case",
            ),
            (
                "[review.programs.a]\nscript = \"s.sh\"\npaths = [\"a\"]\n[review.programs.a]\n",
                "dagq.toml:4: [review.programs.a] is defined twice",
            ),
            (
                "[review.programs.a]\nglobs = [\"a\"]\n",
                "dagq.toml:2: unknown key globs in [review.programs.a]; the keys are script, args, paths, timeout_secs",
            ),
            (
                "[review.programs.a]\npaths = [\"a\"]\npaths = [\"b\"]\n",
                "dagq.toml:3: paths is defined twice",
            ),
            (
                "[review.programs.a]\ncommand = [\"cargo\", \"fmt\"]\npaths = [\"a\"]\n",
                "dagq.toml:2: [review.programs.a] has command, which is not read: write the program as a repository script with script",
            ),
            (
                "[review.programs.a]\nscript = \"s.sh\"\ncommand = [\"x\"]\npaths = [\"a\"]\n",
                "dagq.toml:3: [review.programs.a] has command",
            ),
            (
                "[review.programs.a]\nscript = \"/bin/sh\"\n",
                "dagq.toml:2: value of script",
            ),
            (
                "[review.programs.a]\nscript = \"scripts/*.sh\"\n",
                "dagq.toml:2: value of script",
            ),
            (
                "[review.programs.a]\nscript = \"s.sh\"\npaths = []\n",
                "dagq.toml:3: paths of [review.programs.a] names no glob",
            ),
            (
                "[review.programs.a]\nscript = \"s.sh\"\ntimeout_secs = 0\n",
                "dagq.toml:3: value of timeout_secs",
            ),
            (
                "[review.programs.a]\nscript = \"s.sh\"\n[run.env]\n",
                "dagq.toml:1: [review.programs.a] has no paths",
            ),
            (
                "[review.programs.a]\npaths = [\"a\"]\n",
                "dagq.toml:1: [review.programs.a] has no script",
            ),
            (
                "[review.programs.a]\nargs = [\"y\"]\npaths = [\"a\"]\n",
                "dagq.toml:1: [review.programs.a] has no script",
            ),
        ] {
            let found = format!("{:#}", parse_config(text).unwrap_err());
            assert!(found.contains(error), "{text}: {found}");
        }
        let verifier = ShellVerifier {
            checkout: PathBuf::from("/nonexistent"),
            db: PathBuf::from("/nonexistent/queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier
                .review_programs_in("[review.programs.a]\nscript = \"s.sh\"\npaths = [\"a\"]\n")
                .unwrap()
                .programs
                .len(),
            1
        );
        assert!(verifier.review_programs_in("[nope]\n").is_err());
    }

    /// `[review.programs."builtin:<name>"]` sets a built-in program's
    /// `enabled` and `paths` (ADR-t2259-1 decision 4), apart from the
    /// project's programs; `script` and `args` in it, a name of the prefix
    /// that is no built-in's, and a project's program named with the
    /// prefix are mistakes naming the line.
    #[test]
    fn parses_the_builtin_review_programs() {
        let config = parse_config(
            "[review.programs.\"builtin:secrets\"]\nenabled = true # on\npaths = [\"src/**\", \"src/**\"]\n\
             [review.programs.fmt]\nscript = \"scripts/fmt.sh\"\npaths = [\"src/**\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.review_builtin_programs,
            [BuiltinProgramSetting {
                program: BuiltinProgram::Secrets,
                enabled: Some(true),
                paths: Some(vec!["src/**".to_owned()]),
            }]
        );
        assert_eq!(config.review_programs.len(), 1);
        let names: Vec<String> = config
            .review_programs()
            .effective()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["builtin:secrets", "fmt"]);
        let off = parse_config("[review.programs.'builtin:secrets']\nenabled = false\n").unwrap();
        assert!(off.review_programs().effective().is_empty());
        let header = parse_config("[review.programs.\"builtin:secrets\"]\n[run.env]\n").unwrap();
        assert_eq!(header.review_builtin_programs[0].enabled, None);
        for (text, expected) in [
            (
                "[review.programs.\"builtin:secrets\"]\nscript = \"s.sh\"\n",
                "dagq.toml:2: [review.programs.\"builtin:secrets\"] has script, which a built-in does not take",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\nargs = [\"x\"]\n",
                "dagq.toml:2: [review.programs.\"builtin:secrets\"] has args, which a built-in does not take",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\ntimeout_secs = 5\n",
                "dagq.toml:2: unknown key timeout_secs in [review.programs.\"builtin:secrets\"]; the keys are enabled and paths",
            ),
            (
                "[review.programs.\"builtin:scope\"]\nenabled = true\n",
                "dagq.toml:1: [review.programs.\"builtin:scope\"]: builtin:scope is no built-in program",
            ),
            (
                "[review.programs.\"builtin:my-check\"]\nscript = \"s.sh\"\npaths = [\"a\"]\n",
                "names starting with builtin: are the built-ins', so a project's program may not take one",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\npaths = []\n",
                "dagq.toml:2: paths of [review.programs.\"builtin:secrets\"] names no glob",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\nenabled = 1\n",
                "dagq.toml:2: value of enabled",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\nenabled = true\nenabled = false\n",
                "dagq.toml:3: enabled is defined twice",
            ),
            (
                "[review.programs.\"builtin:secrets\"]\n[review.programs.\"builtin:secrets\"]\n",
                "dagq.toml:2: [review.programs.\"builtin:secrets\"] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    /// `[review.code] paths` names the repository's code paths
    /// (ADR-t2599-1); without the table or with no glob there are none,
    /// and a mistake in it is refused with its line.
    #[test]
    fn parses_the_code_paths_of_the_review_code_table() {
        let config = parse_config(
            "[review.jobs]\nconcurrency = 2\n[review.code]\npaths = [\"src/**\", 'tests/**', \"src/**\"] # code\n",
        )
        .unwrap();
        assert_eq!(
            config.review_code,
            CodePaths::of(vec!["src/**".to_owned(), "tests/**".to_owned()])
        );
        assert_eq!(parse_config("").unwrap().review_code, None);
        assert_eq!(parse_config("[review.code]\n").unwrap().review_code, None);
        assert_eq!(
            parse_config("[review.code]\npaths = []\n")
                .unwrap()
                .review_code,
            None
        );
        for (text, expected) in [
            (
                "[review.code]\nglobs = [\"a\"]\n",
                "dagq.toml:2: unknown key globs in [review.code]; the key is paths",
            ),
            (
                "[review.code]\npaths = [\"a\"]\npaths = [\"b\"]\n",
                "dagq.toml:3: paths is defined twice",
            ),
            (
                "[review.code]\npaths = \"src/**\"\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[review.code]\npaths = [\"/src/**\"]\n",
                "dagq.toml:2: value of paths",
            ),
            (
                "[review.code]\n[review.code]\n",
                "dagq.toml:2: [review.code] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let verifier = ShellVerifier {
            checkout: PathBuf::from("/nonexistent"),
            db: PathBuf::from("/nonexistent/queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier
                .review_code_paths_in("[review.code]\npaths = [\"lib/**\"]\n")
                .unwrap(),
            CodePaths::of(vec!["lib/**".to_owned()])
        );
        assert!(verifier.review_code_paths_in("[nope]\n").is_err());
    }

    /// `[review] revise_limit` is the run's revise limit (ADR-t3033-1): a
    /// whole number of at least 1, none without it (the default); 0, a
    /// negative value and a value that is not a whole number are mistakes
    /// refused with their line, never the default.
    #[test]
    fn parses_the_revise_limit_of_the_review_table() {
        let config = parse_config(
            "[review]\nrevise_limit = 4 # more\n[review.code]\npaths = [\"src/**\"]\n",
        )
        .unwrap();
        assert_eq!(config.review_revise_limit, ReviseLimit::new(4));
        assert_eq!(parse_config("").unwrap().review_revise_limit, None);
        assert_eq!(
            parse_config("[review]\n").unwrap().review_revise_limit,
            None
        );
        for (text, expected) in [
            (
                "[review]\nrevise_limit = 0\n",
                "dagq.toml:2: value of revise_limit",
            ),
            ("[review]\nrevise_limit = 0\n", "positive number, not 0"),
            ("[review]\nrevise_limit = -1\n", "positive number, not -1"),
            ("[review]\nrevise_limit = 1.5\n", "whole number"),
            ("[review]\nrevise_limit = \"3\"\n", "whole number"),
            ("[review]\nrevise_limit = \n", "missing value"),
            (
                "[review]\nrevise_limit = 1\nrevise_limit = 2\n",
                "dagq.toml:3: revise_limit is defined twice",
            ),
            (
                "[review]\nlimit = 1\n",
                "dagq.toml:2: unknown key limit in [review]; the key is revise_limit",
            ),
            (
                "[review]\n[review]\n",
                "dagq.toml:2: [review] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let verifier = ShellVerifier {
            checkout: PathBuf::from("/nonexistent"),
            db: PathBuf::from("/nonexistent/queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier
                .review_revise_limit_in("[review]\nrevise_limit = 5\n")
                .unwrap(),
            ReviseLimit::new(5)
        );
        assert_eq!(verifier.review_revise_limit_in("").unwrap(), None);
        assert!(
            verifier
                .review_revise_limit_in("[review]\nrevise_limit = 0\n")
                .is_err()
        );
    }

    /// The plan review's limit does not follow the run's revise limit a
    /// `dagq.toml` writes (ADR-t3033-1 decision 2, ADR-t2599-1 decision
    /// 9): a proposal sent back [`crate::domain::MAX_PLAN_REVISES`] times
    /// is a concern however high or low `[review] revise_limit` is.
    #[test]
    fn the_plan_reviews_limit_does_not_follow_the_revise_limit() {
        use crate::domain::plan_review::{PlanReviewDecision, PlanReviewVerdict, decide_verdict};
        use crate::domain::{MAX_PLAN_REVISES, ProposalId};
        let revise = PlanReviewVerdict::parse(
            r#"{"verdict":"revise","reasons":["still vague"],"summary":"vague"}"#,
        )
        .unwrap();
        for limit in [1, 5] {
            let config = parse_config(&format!("[review]\nrevise_limit = {limit}\n")).unwrap();
            assert_eq!(config.review_revise_limit, ReviseLimit::new(limit));
            let before = decide_verdict(ProposalId::new(1), &revise, MAX_PLAN_REVISES - 1);
            assert_eq!(before.decision, PlanReviewDecision::Revise, "{limit}");
            let past = decide_verdict(ProposalId::new(1), &revise, MAX_PLAN_REVISES);
            assert_eq!(past.decision, PlanReviewDecision::Concern, "{limit}");
        }
        assert_eq!(MAX_PLAN_REVISES, 2);
    }

    /// Declarations resolve their time limit and reject ambiguous or
    /// unusable commands without changing paths-only configuration.
    #[test]
    fn parses_the_e2e_command_and_timeout() {
        let config =
            parse_config("[e2e]\ntimeout_secs = 42\ncommand = 'sh check.sh'\npaths = ['src/**']\n")
                .unwrap();
        let declared = config.e2e_command.unwrap();
        assert_eq!(declared.command, "sh check.sh");
        assert_eq!(declared.timeout, Duration::from_secs(42));
        assert_eq!(config.e2e_paths, ["src/**"]);
        assert_eq!(
            parse_config("[e2e]\ncommand = 'check'\n")
                .unwrap()
                .e2e_command
                .unwrap()
                .timeout,
            crate::application::install::E2E_TIMEOUT
        );
        assert!(
            parse_config("[e2e]\npaths = ['src/**']\n")
                .unwrap()
                .e2e_command
                .is_none()
        );
        assert_eq!(
            parse_config("[e2e]\ncommand = 'check'\ntimeout_secs = 9223372036854775807\n")
                .unwrap()
                .e2e_command
                .unwrap()
                .timeout,
            Duration::from_secs(i64::MAX as u64)
        );
        for fields in [
            "command = ''",
            "command = '   '",
            "command = \"a\\nb\"",
            "command = \"a\\rb\"",
            "command = 1",
            "command = 'check'\ntimeout_secs = 0",
            "timeout_secs = 42",
            "command = 'a'\ncommand = 'b'",
            "command = 'a'\ntimeout_secs = 1\ntimeout_secs = 2",
            "unknown = 'x'",
        ] {
            assert!(
                parse_config(&format!("[e2e]\n{fields}\n")).is_err(),
                "{fields}"
            );
        }
    }

    /// Every start reads the current working file; invalid or unreadable
    /// files do not turn into an absent declaration.
    #[test]
    fn reloads_the_e2e_declaration_and_preserves_read_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE_NAME);
        assert!(load_e2e_command(dir.path()).unwrap().is_none());
        for command in ["first", "second"] {
            fs::write(&path, format!("[e2e]\ncommand = '{command}'\n")).unwrap();
            assert_eq!(
                load_e2e_command(dir.path()).unwrap().unwrap().command,
                command
            );
        }
        fs::write(&path, "[e2e]\ncommand = ''\n").unwrap();
        assert!(load_e2e_command(dir.path()).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_e2e_command(dir.path()).is_err());
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

    /// `[egress] registries` names the package registries the queue
    /// broker opens a tunnel to (ADR-t2257-1 decision 3): none by default,
    /// and a mistake in it is refused with its line.
    #[test]
    fn parses_the_registries_of_the_egress_table() {
        let config = parse_config(
            "[egress]\n# the index\nregistries = [\"Crates.io\", 'index.crates.io:8443', \"crates.io:443\"] # why\n",
        )
        .unwrap();
        assert_eq!(
            config
                .egress_registries
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["crates.io", "index.crates.io:8443"]
        );
        assert!(
            parse_config("[egress]\n")
                .unwrap()
                .egress_registries
                .is_empty()
        );
        assert!(parse_config("").unwrap().egress_registries.is_empty());
        for (text, expected) in [
            (
                "[egress]\nhosts = [\"a\"]\n",
                "dagq.toml:2: unknown key hosts",
            ),
            (
                "[egress]\nregistries = [\"a\"]\nregistries = [\"b\"]\n",
                "dagq.toml:3: registries is defined twice",
            ),
            (
                "[egress]\nregistries = \"crates.io\"\n",
                "dagq.toml:2: value of registries",
            ),
            (
                "[egress]\nregistries = [\"https://crates.io\"]\n",
                "not a host",
            ),
            (
                "[egress]\nregistries = [\"*.crates.io\"]\n",
                "dagq.toml:2: value of registries",
            ),
            (
                "[egress]\n[egress]\n",
                "dagq.toml:2: [egress] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(load_egress_registries(dir.path()).unwrap().is_empty());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[egress]\nregistries = [\"crates.io\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_egress_registries(dir.path()).unwrap()[0].to_string(),
            "crates.io"
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[egress]\nregistries = 1\n",
        )
        .unwrap();
        assert!(load_egress_registries(dir.path()).is_err());
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

    /// `[tasks] title_prefixes` names the prefixes of a task's title; an
    /// empty list, an empty prefix or one with a ':' is refused.
    #[test]
    fn parses_the_title_prefixes_of_the_tasks_table() {
        let config =
            parse_config("[tasks]\nchanges = [\"fix\"]\ntitle_prefixes = [\"feat\", 'fix'] # c\n")
                .unwrap();
        assert_eq!(config.title_prefixes.unwrap().values(), ["feat", "fix"]);
        assert_eq!(parse_config("[tasks]\n").unwrap().title_prefixes, None);
        let error = |text: &str| format!("{:#}", parse_config(text).unwrap_err());
        for (text, expected) in [
            ("[tasks]\ntitle_prefixes = []\n", "names no prefix"),
            (
                "[tasks]\ntitle_prefixes = [\"feat\", \"\"]\n",
                "names an empty prefix",
            ),
            ("[tasks]\ntitle_prefixes = [\"feat:\"]\n", "':'"),
            ("[tasks]\ntitle_prefixes = \"feat\"\n", "expected an array"),
            (
                "[tasks]\ntitle_prefixes = [\"a\"]\ntitle_prefixes = [\"b\"]\n",
                "defined twice",
            ),
            ("[tasks]\nprefixes = [\"a\"]\n", "unknown key prefixes"),
        ] {
            let got = error(text);
            assert!(got.contains(expected), "{text}: {got}");
            assert!(got.contains("dagq.toml:"), "{text}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_title_prefixes(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[tasks]\ntitle_prefixes = [\"chore\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_title_prefixes(dir.path()).unwrap().unwrap().values(),
            ["chore"]
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[tasks]\ntitle_prefixes = []\n",
        )
        .unwrap();
        assert!(load_title_prefixes(dir.path()).is_err());
    }

    /// `[tasks] related_docs` names the globs of `lint`'s related
    /// documents, each once; an empty list is taken and a glob `add
    /// --paths` refuses is refused.
    #[test]
    fn parses_the_related_docs_of_the_tasks_table() {
        let config = parse_config(
            "[tasks]\nrelated_docs = [\"docs/design/**/*.md\", 'README.md', \"README.md\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.related_docs.unwrap(),
            ["docs/design/**/*.md", "README.md"]
        );
        assert_eq!(parse_config("[tasks]\n").unwrap().related_docs, None);
        assert_eq!(
            parse_config("[tasks]\nrelated_docs = []\n")
                .unwrap()
                .related_docs,
            Some(vec![])
        );
        let error = |text: &str| format!("{:#}", parse_config(text).unwrap_err());
        for (text, expected) in [
            ("[tasks]\nrelated_docs = [\"/docs\"]\n", "relative"),
            ("[tasks]\nrelated_docs = [\"../x.md\"]\n", ".."),
            ("[tasks]\nrelated_docs = \"docs\"\n", "expected an array"),
            (
                "[tasks]\nrelated_docs = [\"a\"]\nrelated_docs = [\"b\"]\n",
                "defined twice",
            ),
        ] {
            let got = error(text);
            assert!(got.contains(expected), "{text}: {got}");
            assert!(got.contains("dagq.toml:"), "{text}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(load_related_docs(dir.path()).unwrap().is_empty());
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[tasks]\nrelated_docs = [\"docs/*.md\"]\n",
        )
        .unwrap();
        assert_eq!(load_related_docs(dir.path()).unwrap(), ["docs/*.md"]);
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

    /// `[container.verification]` declares the command that prepares a
    /// verification's materials on the host; without it, none runs.
    #[test]
    fn the_verification_table_declares_the_preparation_command() {
        let config =
            parse_config("[container.verification]\nprepare = \"sh scripts/p.sh\"\n").unwrap();
        assert_eq!(
            config.container_verification_prepare.as_deref(),
            Some("sh scripts/p.sh")
        );
        assert_eq!(
            parse_config("").unwrap().container_verification_prepare,
            None
        );
        for (text, error) in [
            ("[container.verification]\n", "has no prepare"),
            ("[container.verification]\nprepare = ' '\n", "blank"),
            (
                "[container.verification]\nprepare = 'a'\nprepare = 'b'\n",
                "defined twice",
            ),
            ("[container.verification]\nrun = 'a'\n", "unknown key run"),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
    }

    /// `[container.build_output]` names the container's directory the run's
    /// build output volume is mounted at; without it, none is.
    #[test]
    fn the_build_output_table_names_a_directory_of_the_container() {
        let config = parse_config("[container.build_output]\ndir = \"/build/target\"\n").unwrap();
        assert_eq!(
            config.container_build_output.as_deref(),
            Some("/build/target")
        );
        assert_eq!(parse_config("").unwrap().container_build_output, None);
        for (text, error) in [
            ("[container.build_output]\n", "has no dir"),
            (
                "[container.build_output]\ndir = 'build'\n",
                "not a directory of the container",
            ),
            (
                "[container.build_output]\ndir = '/'\n",
                "not a directory of the container",
            ),
            (
                "[container.build_output]\ndir = '/a,ro'\n",
                "not a directory of the container",
            ),
            (
                "[container.build_output]\ndir = '/a/../b'\n",
                "not a directory of the container",
            ),
            (
                "[container.build_output]\ndir = '/a'\ndir = '/b'\n",
                "defined twice",
            ),
            (
                "[container.build_output]\npath = '/a'\n",
                "unknown key path",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
    }

    /// `[container.worker_cache]` lists the container's directories the
    /// queue's worker cache is mounted at; without it, or empty, none is,
    /// and a directory that could not be mounted as written is refused as
    /// an error of `dagq.toml`'s line.
    #[test]
    fn the_worker_cache_table_lists_directories_of_the_container() {
        let config = parse_config(
            "[container.worker_cache]\ndirs = [\"/cache/registry\", \"/cache/compiler\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.container_worker_cache,
            ["/cache/registry", "/cache/compiler"]
        );
        assert!(parse_config("").unwrap().container_worker_cache.is_empty());
        assert!(
            parse_config("[container.worker_cache]\ndirs = []\n")
                .unwrap()
                .container_worker_cache
                .is_empty()
        );
        for (text, error) in [
            ("[container.worker_cache]\n", "has no dirs"),
            (
                "[container.worker_cache]\ndirs = ['cache']\n",
                "dagq.toml:2: value of dirs: \"cache\" is not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/']\n",
                "not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/a,ro']\n",
                "not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/a:/b']\n",
                "not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/a/../b']\n",
                "not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/a', '/a/']\n",
                "listed twice",
            ),
            (
                "[container.worker_cache]\ndirs = ['/cache/a..b']\n",
                "not a directory of the container",
            ),
            (
                "[container.worker_cache]\ndirs = ['/build']\n[container.build_output]\ndir = '/build'\n",
                "the directory of [container.build_output]",
            ),
            (
                "[container.worker_cache]\ndirs = ['/a']\ndirs = ['/b']\n",
                "defined twice",
            ),
            ("[container.worker_cache]\ndir = '/a'\n", "unknown key dir"),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
    }

    /// `[container.image]` names the project's Containerfile and its
    /// inputs, repository paths only; without it the base runs alone.
    #[test]
    fn the_container_image_names_a_containerfile_and_its_inputs_in_the_repository() {
        assert_eq!(parse_config("").unwrap().container_image, None);
        let config = parse_config(
            "[container.image]\ncontainerfile = \".dagq/container/Containerfile\" # dogfooding\ninputs = [\"rust-toolchain.toml\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.container_image,
            Some(ProjectImage {
                containerfile: ".dagq/container/Containerfile".into(),
                inputs: vec!["rust-toolchain.toml".into()],
            })
        );
        let alone = parse_config("[container.image]\ncontainerfile = 'Containerfile'\n").unwrap();
        assert_eq!(alone.container_image.unwrap().inputs, Vec::<String>::new());
        for (text, error) in [
            ("[container.image]\ninputs = []\n", "has no containerfile"),
            (
                "[container.image]\ncontainerfile = '/etc/Containerfile'\n",
                "not a relative repository path",
            ),
            (
                "[container.image]\ncontainerfile = 'C'\ninputs = ['../secret']\n",
                "inside the repository",
            ),
            (
                "[container.image]\ncontainerfile = 'C'\ncontainerfile = 'D'\n",
                "defined twice",
            ),
            ("[container.image]\ntag = 'x'\n", "unknown key tag"),
            (
                "[container.image]\ncontainerfile = 'C'\n[container.image]\n",
                "defined twice",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
    }

    /// `runtime` of `[roles.worker]` and `[roles.verification]` picks the
    /// host or the container, the host when unset; a container anywhere
    /// else is refused with why (goal 38, ADR-t2113-3 decisions 1 to 3).
    /// Reading the file is the integration tests' (`runtime_container`).
    #[test]
    fn the_worker_and_the_verification_choose_their_runtime_and_no_other_role_a_container() {
        use crate::domain::actor_model::ModelRole;
        assert_eq!(parse_config("").unwrap().runtimes, RuntimeConfig::default());
        assert_eq!(
            parse_config(
                "[roles.worker]
[roles.verification]
"
            )
            .unwrap()
            .runtimes,
            RuntimeConfig::default()
        );
        let config = parse_config(
            "[roles.worker]\nruntime = \"container\" # isolated\n[roles.verification]\nruntime = 'host'\n[roles.review]\nruntime = \"host\"\nprovider = 'codex'\n",
        )
        .unwrap();
        assert_eq!(
            config.runtimes,
            RuntimeConfig {
                worker: Runtime::Container,
                verification: Runtime::Host,
            }
        );
        // `runtime = "host"` gives the role nothing else.
        assert_eq!(
            config.roles.provider(ModelRole::Review).0,
            crate::domain::Provider::Codex
        );
        let config = parse_config("[roles.verification]\nruntime = 'container'\n").unwrap();
        assert_eq!(config.runtimes.verification, Runtime::Container);
        assert_eq!(config.runtimes.worker, Runtime::Host);
        for role in ModelRole::ALL {
            let text = format!("[roles.{}]\nruntime = 'container'\n", role.as_str());
            let error = format!("{:#}", parse_config(&text).unwrap_err());
            assert!(
                error.contains("only for [roles.worker] and [roles.verification]")
                    && error.contains(&format!("[roles.{}]", role.as_str())),
                "{text:?}: {error}"
            );
        }
        for (text, expected) in [
            (
                "[roles.worker]\nruntime = 'podman'",
                "unknown runtime \"podman\"",
            ),
            (
                "[roles.worker]\nprovider = 'claude'",
                "its only key is runtime",
            ),
            (
                "[roles.worker]\nruntime = 'host'\nruntime = 'host'",
                "is defined twice",
            ),
            ("[roles.worker]\n[roles.worker]", "is defined twice"),
            ("[roles.review]\nruntime = 'docker'", "unknown runtime"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    /// A queue bound to no checkout reads the host for every target; a
    /// binding that could not be resolved fails every read (goal 38).
    #[test]
    fn the_bound_runtimes_are_the_host_without_a_checkout_and_fail_without_its_binding() {
        assert_eq!(
            BoundRuntimes::new(Ok(None)).read().unwrap(),
            RuntimeConfig::default()
        );
        let bound = BoundRuntimes::new(Err(anyhow::anyhow!("repository moved")));
        for _ in 0..2 {
            let error = format!("{:#}", bound.read().unwrap_err());
            assert!(error.contains("repository moved"), "{error}");
        }
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
                window: 30,
                ..WorkerTrial::default()
            }
        );
        let set = parse_config(
            "[worker.trial]\nenabled = true\nname = \"ab-1\"\ntreatment_percent = 10 # small\n\
             control_model = \"claude-opus-5-5\"\ncontrol_effort = \"high\"\n\
             treatment_model = \"claude-opus-5-5\"\ntreatment_effort = \"medium\"\nlimited = false\n",
        )
        .unwrap()
        .worker_trial;
        assert_eq!(
            (
                set.name.as_str(),
                set.treatment_percent,
                set.control.effort.as_str(),
                set.treatment.model.as_str(),
                set.limited
            ),
            ("ab-1", 10, "high", "claude-opus-5-5", false)
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
                "[worker.trial]\ntreatment_percent = 101",
                "whole percent from 0 to 100",
            ),
            ("[worker.trial]\ntreatment_percent = -1", "whole percent"),
            ("[worker.trial]\nname = \" \"", "is blank"),
            (
                "[worker.trial]\ntreatment_effort = \"high\"",
                "[worker.trial]: the treatment candidate claude-sonnet-5/high is not a step of the ladder",
            ),
            (
                "[worker.trial]\ncontrol_model = \"gpt-5\"",
                "the control candidate gpt-5/medium is not a step",
            ),
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
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert!(verifier.worker_trial().unwrap().enabled);
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[worker.trial]\nx = 1\n").unwrap();
        assert!(load_worker_trial(dir.path()).is_err());
    }

    #[test]
    fn parses_the_provider_trial_table() {
        // Off unless it says so, and the claim as before (ADR-t4005-1).
        assert_eq!(
            parse_config("").unwrap().worker_trial.providers,
            ProviderTrial::default()
        );
        let set = parse_config(
            "[worker.provider_trial]\nenabled = true # on\nname = \"pairs-1\"\n\
             treatment_percent = 20\ntreatment_worker = \"claude\"\ntreatment_review = \"codex\"\n",
        )
        .unwrap()
        .worker_trial;
        assert_eq!(
            set.providers,
            ProviderTrial {
                enabled: true,
                name: "pairs-1".into(),
                treatment_percent: 20,
                treatment_worker: crate::domain::Provider::Claude,
                treatment_review: crate::domain::Provider::Codex,
            }
        );
        // The model trial is left as it was.
        assert!(!set.enabled);
        // A `[worker.trial]` named as the provider trial's default, with no
        // provider trial, is read as before; so is one turned off.
        for text in [
            "[worker.trial]\nenabled = true\nname = \"provider-pair\"\n",
            "[worker.trial]\nname = \"x\"\n[worker.provider_trial]\nname = \"x\"\n",
        ] {
            let config = parse_config(text).unwrap();
            assert!(!config.worker_trial.providers.enabled, "{text}");
        }
        assert_eq!(
            parse_config("[worker.trial]\nenabled = true\nname = \"provider-pair\"\n")
                .unwrap()
                .worker_trial
                .name,
            "provider-pair"
        );
        for (text, error) in [
            (
                "[worker.provider_trial]\nother = 1",
                "unknown key other in [worker.provider_trial]",
            ),
            (
                "[worker.provider_trial]\ntreatment_percent = 101",
                "whole percent from 0 to 100",
            ),
            (
                "[worker.provider_trial]\ntreatment_percent = -5",
                "whole percent",
            ),
            (
                "[worker.provider_trial]\ntreatment_review = \"codex\"",
                "[worker.provider_trial]: treatment_worker and treatment_review are both codex",
            ),
            (
                "[worker.provider_trial]\ntreatment_worker = \"gpt\"",
                "value of treatment_worker",
            ),
            (
                "[worker.provider_trial]\nenabled = true\nname = \"worker\"",
                "is the name of [worker.trial]",
            ),
            (
                "[worker.trial]\nname = \"x\"\n[worker.provider_trial]\nenabled = true\nname = \"x\"",
                "the two trials must draw independently",
            ),
            ("[worker.provider_trial]\nname = \" \"", "is blank"),
            (
                "[worker.provider_trial]\nenabled = true\nenabled = true",
                "enabled is defined twice",
            ),
        ] {
            let message = format!("{:#}", parse_config(text).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
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

    /// The removed resource broker's tables are accepted whatever they
    /// hold, as often as they come, and change nothing else (ADR-t2125-1):
    /// a repository that still has them is not stopped by them.
    #[test]
    fn the_removed_brokers_tables_are_accepted_and_not_read() {
        let text = "[broker]\nmode = \"required\"\nport = 1\nnot a key\n[broker.package]\nfetch = []\n[broker]\n[e2e]\npaths = [\"src/**\"]\n";
        let config = parse_config(text).unwrap();
        assert_eq!(config.e2e_paths, ["src/**"]);
        let without = parse_config("[e2e]\npaths = [\"src/**\"]\n").unwrap();
        assert_eq!(config, without);
        // Another table it does not know still stops it.
        let error = format!("{:#}", parse_config("[brokers]\n").unwrap_err());
        assert!(error.contains("unknown table [brokers]"), "{error}");
        assert!(!error.contains("[broker]"), "{error}");
    }

    #[test]
    fn parses_the_disk_table() {
        // The retired keys of an older `[disk]` are read on, their values
        // not used, whatever they hold.
        let config = parse_config(
            "[disk]\nsample_runs = 5\nclaim_factor = 2.5 # more\nintegrate_factor = x\nmin_free_bytes = 1_000\n",
        )
        .unwrap();
        assert_eq!(config.disk, DiskConfig::default());
        assert_eq!(config.disk.reserve, 0.05);
        assert_eq!(
            config.disk_retired,
            [
                "sample_runs",
                "claim_factor",
                "integrate_factor",
                "min_free_bytes"
            ]
        );
        assert!(parse_config("").unwrap().disk_retired.is_empty());
        let twice = parse_config("[disk]\nsample_runs = 1\nsample_runs = 2\n").unwrap();
        assert_eq!(twice.disk_retired, ["sample_runs"]);
        // The reserve, a share of the volume.
        let config = parse_config("[disk]\nreserve = \"10%\" # more\n").unwrap();
        assert_eq!(config.disk.reserve, 0.10);
        for text in [
            "reserve = 5",
            "reserve = \"0%\"",
            "reserve = \"5\"",
            "reserve = \"x%\"",
        ] {
            let text = format!("[disk]\n{text}");
            let message = format!("{:#}", parse_config(&text).unwrap_err());
            assert!(message.contains("value of reserve"), "{text}: {message}");
        }
        let config =
            parse_config("[disk]\nask_after_secs = x\nask_margin = 0\nclose_margin = -1\n")
                .unwrap();
        assert_eq!(config.disk, DiskConfig::default());
        assert_eq!(
            config.disk_retired,
            ["ask_after_secs", "ask_margin", "close_margin"]
        );
        assert_eq!(parse_config("").unwrap().disk, DiskConfig::default());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_disk_config(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[disk]\nclaim_factor = 3\nask_after_secs = 60\n",
        )
        .unwrap();
        assert_eq!(
            load_disk_config(dir.path()).unwrap().unwrap(),
            DiskConfig::default()
        );
        // Warned of once in a process: the first call says so, the next
        // ones not.
        let _ = warns_of_retired_disk_keys();
        assert!(!warns_of_retired_disk_keys());
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[disk]\nx = 3\n").unwrap();
        assert!(load_disk_config(dir.path()).is_err());
    }

    #[test]
    fn disk_estimate_validates_fixed_sizes_and_ignores_recent_sizes_with_a_warning() {
        use crate::infrastructure::telemetry::Telemetry;
        let default = DiskConfig::default();
        for text in ["", "[disk]", "[disk]\nestimate = \"recent\""] {
            assert_eq!(parse_config(text).unwrap().disk, default);
        }
        let fixed = DiskConfig {
            estimate: DiskEstimate::Fixed,
            run_size: Some(20 << 30),
            landing_size: None,
            ..default
        };
        for text in [
            "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"",
            "[disk]\nrun_size = \"20GiB\"\nestimate = \"fixed\"",
        ] {
            assert_eq!(parse_config(text).unwrap().disk, fixed);
        }
        for (text, line, part) in [
            ("[disk]\nestimate = \"fixed\"", 2, "requires run_size"),
            (
                "[disk]\nestimate = \"fixed\"\nrun_size = \"abc\"",
                3,
                "value of run_size",
            ),
            (
                "[disk]\nestimate = \"fixed\"\nrun_size = \"0GiB\"",
                3,
                "value of run_size",
            ),
            (
                "[disk]\nrun_size = 20\nestimate = \"fixed\"",
                2,
                "value of run_size",
            ),
            ("[disk]\nestimate = \"other\"", 2, "value of estimate"),
            ("[disk]\nestimate = 1", 2, "value of estimate"),
            (
                "[disk]\nestimate = \"recent\"\nestimate = \"fixed\"",
                3,
                "defined twice",
            ),
            ("[disk]\nrun_size = 1\nrun_size = 2", 3, "defined twice"),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(&format!("dagq.toml:{line}:")), "{error}");
            assert!(error.contains(part), "{error}");
        }
        for text in [
            "[disk]\nestimate = \"recent\"\nrun_size = \"20GiB\"",
            "[disk]\nrun_size = \"abc\"\nestimate = \"recent\"",
            "[disk]\nrun_size = \"0GiB\"",
            "[disk]\nrun_size = 123",
        ] {
            let (telemetry, captured) = Telemetry::capture();
            let config = telemetry.in_scope(|| parse_config(text)).unwrap();
            assert_eq!(config.disk, default);
            let output = captured.text();
            assert!(output.contains("run_size is ignored"), "{output}");
            assert!(output.contains("WARN"), "{output}");
        }
    }

    #[test]
    fn waiting_build_cleanup_defaults_and_values_are_independent_of_estimate() {
        use WaitingBuildCleanup::{Eager, Lazy, Never};
        assert_eq!(parse_config("").unwrap().disk.waiting_build_cleanup, Lazy);
        for method in [
            "",
            "estimate = 'recent'\n",
            "estimate = 'fixed'\nrun_size = '20GiB'\n",
        ] {
            assert_eq!(
                parse_config(&format!("[disk]\n{method}"))
                    .unwrap()
                    .disk
                    .waiting_build_cleanup,
                Lazy
            );
            for (value, expected) in [("eager", Eager), ("lazy", Lazy), ("never", Never)] {
                let config = parse_config(&format!(
                    "[disk]\n{method}waiting_build_cleanup = '{value}'"
                ))
                .unwrap();
                assert_eq!(config.disk.waiting_build_cleanup, expected);
            }
        }
        for value in ["'other'", "1", "true"] {
            let error = format!(
                "{:#}",
                parse_config(&format!("[disk]\nwaiting_build_cleanup = {value}")).unwrap_err()
            );
            assert!(error.contains("dagq.toml:2:"), "{error}");
            assert!(error.contains("value of waiting_build_cleanup"), "{error}");
        }
    }

    #[test]
    fn landing_size_defaults_to_worker_and_validates_or_warns_by_method() {
        use crate::domain::disk::{landing_expectation, run_expectation};
        use crate::infrastructure::telemetry::Telemetry;
        let prefix = "[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\n";
        let config = parse_config(prefix).unwrap().disk;
        assert_eq!(
            landing_expectation(&config, run_expectation(&config, &[], 0)),
            Some(20 << 30)
        );
        let config = parse_config(&format!("{prefix}landing_size = \"30GiB\"\n"))
            .unwrap()
            .disk;
        assert_eq!(
            landing_expectation(&config, run_expectation(&config, &[], 0)),
            Some(30 << 30)
        );
        for value in ["\"abc\"", "\"0GiB\"", "123"] {
            let error = format!(
                "{:#}",
                parse_config(&format!("{prefix}landing_size = {value}")).unwrap_err()
            );
            assert!(error.contains("dagq.toml:4:"), "{error}");
            assert!(error.contains("value of landing_size"), "{error}");
        }
        for value in ["\"30GiB\"", "\"abc\"", "\"0GiB\"", "123"] {
            let (telemetry, captured) = Telemetry::capture();
            let config = telemetry
                .in_scope(|| {
                    parse_config(&format!(
                        "[disk]\nlanding_size = {value}\nestimate = \"recent\""
                    ))
                })
                .unwrap();
            assert_eq!(config.disk, DiskConfig::default());
            assert!(captured.text().contains("WARN"));
            assert!(
                captured
                    .text()
                    .contains("dagq.toml:2: [disk] landing_size is ignored")
            );
        }
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
            ("[recheck]\npaths = []", "2: paths is empty"),
            ("[recheck]\npaths = 'src/**'", "2: value of paths"),
            ("\n[recheck]\npaths = ['/src/**']", "3: value of paths"),
            ("[recheck]\npaths = ['src/../x']", "2: value of paths"),
            (
                "[recheck]\npaths = ['a']\npaths = ['b']",
                "3: paths is defined twice",
            ),
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
                required_jobs: Vec::new(),
                scheduled_workflows: Vec::new(),
                freshness: Vec::new(),
            })
        );
        assert_eq!(
            parse_config(
                "[ci_watch]\nworkflow = 'CI'\nbranch = 'trunk'\ninterval_secs = 60 # a minute\njunit_artifacts = ['junit-*', 'more'] # globs\nrequired_jobs = ['Rust (macOS)', 'linux']\nscheduled_workflows = ['it-budget.yml', 'Stress']\n"
            )
            .unwrap()
            .ci_watch,
            Some(CiWatchConfig {
                workflow: "CI".into(),
                branch: Some("trunk".into()),
                interval_secs: 60,
                junit_artifacts: vec!["junit-*".into(), "more".into()],
                required_jobs: vec!["Rust (macOS)".into(), "linux".into()],
                scheduled_workflows: vec!["it-budget.yml".into(), "Stress".into()],
                freshness: Vec::new(),
            })
        );
        for (text, expected) in [
            ("[ci_watch]\n", "dagq.toml:1: [ci_watch] has no workflow"),
            (
                "[ci_watch]\nworkflow = 'a'\nkind = 'b'\n",
                "dagq.toml:3: unknown key kind in [ci_watch]; the keys are workflow, branch, interval_secs, junit_artifacts, required_jobs, scheduled_workflows",
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
                "[ci_watch]\nworkflow = 'a'\nrequired_jobs = 'x'\n",
                "dagq.toml:3: value of required_jobs",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nrequired_jobs = [' ']\n",
                "dagq.toml:3: required_jobs has a blank job name",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nrequired_jobs = ['x', 'x']\n",
                "dagq.toml:3: required_jobs names \"x\" twice",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nscheduled_workflows = 'x'\n",
                "dagq.toml:3: value of scheduled_workflows",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nscheduled_workflows = [' ']\n",
                "dagq.toml:3: scheduled_workflows has a blank workflow",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\nscheduled_workflows = ['x', 'x']\n",
                "dagq.toml:3: scheduled_workflows names \"x\" twice",
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

    /// `[[ci_watch.freshness]]` (ADR-t3924-1) declares the workflows whose
    /// latest success is watched for its age, each with its limit and the
    /// lead before it (a quarter of the limit when left out); a
    /// `[ci_watch]` without one watches none, and its mistakes are errors
    /// with their line.
    #[test]
    fn parses_the_freshness_tables_of_ci_watch() {
        use crate::domain::ci_watch::freshness::FreshnessWatch;
        let without = parse_config("[ci_watch]\nworkflow = 'ci.yml'\n")
            .unwrap()
            .ci_watch
            .unwrap();
        assert_eq!(without.freshness, Vec::new());
        let watch = parse_config(
            "[ci_watch]\nworkflow = 'ci.yml'\nscheduled_workflows = ['b.yml']\n\n[[ci_watch.freshness]]\nworkflow = 'table.yml'\nmax_age_hours = 48 # two days\nwarn_before_hours = 12\n\n[[ci_watch.freshness]] # another\nworkflow = 'Other'\nmax_age_hours = 10\n\n[supervisor]\nparallel = 2\n",
        )
        .unwrap()
        .ci_watch
        .unwrap();
        assert_eq!(watch.workflow, "ci.yml");
        assert_eq!(watch.scheduled_workflows, vec!["b.yml".to_owned()]);
        assert_eq!(
            watch.freshness,
            vec![
                FreshnessWatch {
                    workflow: "table.yml".into(),
                    max_age_secs: 48 * 3600,
                    lead_secs: 12 * 3600,
                },
                FreshnessWatch {
                    workflow: "Other".into(),
                    max_age_secs: 10 * 3600,
                    lead_secs: 9000,
                },
            ]
        );
        // The largest limit whose seconds an age in milliseconds can reach
        // is read as it is.
        let largest = parse_config(
            "[ci_watch]\nworkflow = 'a'\n[[ci_watch.freshness]]\nworkflow = 't'\nmax_age_hours = 2562047788015\nwarn_before_hours = 2562047788014\n",
        )
        .unwrap()
        .ci_watch
        .unwrap()
        .freshness;
        assert_eq!(
            largest,
            vec![FreshnessWatch {
                workflow: "t".into(),
                max_age_secs: 2_562_047_788_015 * 3600,
                lead_secs: 2_562_047_788_014 * 3600,
            }]
        );
        let base = "[ci_watch]\nworkflow = 'a'\n[[ci_watch.freshness]]\n";
        for (text, expected) in [
            (
                "[[ci_watch.freshness]]\nworkflow = 't'\nmax_age_hours = 1\n".to_owned(),
                "dagq.toml:1: [[ci_watch.freshness]] needs [ci_watch]",
            ),
            (
                "[ci_watch]\nworkflow = 'a'\n[ci_watch.freshness]\n".to_owned(),
                "dagq.toml:3: [ci_watch.freshness] holds one workflow per table",
            ),
            (
                format!("{base}max_age_hours = 1\n"),
                "dagq.toml:3: [[ci_watch.freshness]] has no workflow",
            ),
            (
                format!("{base}workflow = 't'\n"),
                "dagq.toml:3: [[ci_watch.freshness]] has no max_age_hours",
            ),
            (
                format!("{base}workflow = 't'\nbranch = 'x'\n"),
                "dagq.toml:5: unknown key branch in [[ci_watch.freshness]]; the keys are workflow, max_age_hours, warn_before_hours",
            ),
            (
                format!("{base}workflow = 't'\nworkflow = 'u'\n"),
                "dagq.toml:5: workflow is defined twice",
            ),
            (
                format!("{base}workflow = ' '\n"),
                "dagq.toml:4: workflow is blank",
            ),
            (
                format!("{base}workflow = 't'\nmax_age_hours = 0\n"),
                "dagq.toml:5: value of max_age_hours",
            ),
            (
                format!("{base}workflow = 't'\nmax_age_hours = 4\nwarn_before_hours = 'x'\n"),
                "dagq.toml:6: value of warn_before_hours",
            ),
            (
                format!("{base}workflow = 't'\nmax_age_hours = 4\nwarn_before_hours = 4\n"),
                "dagq.toml:3: warn_before_hours of [[ci_watch.freshness]] must be less than max_age_hours (4), not 4",
            ),
            (
                format!(
                    "{base}workflow = 't'\nmax_age_hours = 6000000000000000\nwarn_before_hours = 5999999999999999\n"
                ),
                "dagq.toml:5: max_age_hours is too large: 6000000000000000 hours",
            ),
            (
                format!(
                    "{base}workflow = 't'\nmax_age_hours = 2562047788015\nwarn_before_hours = 2562047788016\n"
                ),
                "dagq.toml:6: warn_before_hours is too large: 2562047788016 hours",
            ),
            (
                format!(
                    "{base}workflow = 't'\nmax_age_hours = 4\n[[ci_watch.freshness]]\nworkflow = 't'\nmax_age_hours = 5\n"
                ),
                "dagq.toml:6: [[ci_watch.freshness]] repeats workflow \"t\" of line 3",
            ),
        ] {
            let error = format!("{:#}", parse_config(&text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
    }

    #[test]
    fn non_incremental_landing_config_rejects_missing_support_and_bad_values() {
        assert!(parse_config("[toolchains.rust]\nprofile = 'rust'\n[landing_verification]\nnon_incremental = true\n").unwrap().landing_verification.unwrap().replaces.is_empty());
        let table = "[landing_verification]\nreplaces = ['gate']\ncommand = 'landing'\n";
        for value in ["true", "'true'"] {
            assert!(parse_config(&format!("{table}non_incremental = {value}\n")).is_err());
        }
        assert!(parse_config(&format!("{table}non_incremental = false\n")).is_ok());
        let config =
            format!("{table}non_incremental = true\n[toolchains.rust]\nprofile = 'rust'\n");
        assert!(
            parse_config(&config)
                .unwrap()
                .landing_verification
                .unwrap()
                .non_incremental
        );
        assert!(
            parse_config(&format!(
                "{table}non_incremental = true\nnon_incremental = false\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn parses_the_landing_verification_table() {
        assert_eq!(parse_config("").unwrap().landing_verification, None);
        assert_eq!(
            parse_config(
                "[landing_verification]\nreplaces = ['cargo llvm-cov nextest'] # the gate\ncommand = \"sh scripts/landing.sh\"\n"
            )
            .unwrap()
            .landing_verification,
            Some(LandingVerification {
                non_incremental: false,
                replaces: vec!["cargo llvm-cov nextest".into()],
                command: "sh scripts/landing.sh".into(),
            })
        );
        for (text, expected) in [
            (
                "[landing_verification]\ncommand = 'x'\n",
                "dagq.toml:1: [landing_verification] has no replaces",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\n",
                "dagq.toml:1: [landing_verification] has no command",
            ),
            (
                "[landing_verification]\nreplaces = []\ncommand = 'x'\n",
                "dagq.toml:2: replaces is empty",
            ),
            (
                "[landing_verification]\nreplaces = [' ']\ncommand = 'x'\n",
                "dagq.toml:2: replaces has a blank text",
            ),
            (
                "[landing_verification]\nreplaces = ['a', 'a']\ncommand = 'x'\n",
                "dagq.toml:2: replaces names \"a\" twice",
            ),
            (
                "[landing_verification]\nreplaces = 'a'\ncommand = 'x'\n",
                "dagq.toml:2: value of replaces",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\ncommand = ' '\n",
                "dagq.toml:3: command is blank",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\ncommand = 'x'\ncommand = 'y'\n",
                "dagq.toml:4: command is defined twice",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\nreplaces = ['b']\n",
                "dagq.toml:3: replaces is defined twice",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\ncommand = 'x'\nmode = 'y'\n",
                "dagq.toml:4: unknown key mode in [landing_verification]; the keys are replaces, command, non_incremental",
            ),
            (
                "[landing_verification]\nreplaces = ['a']\ncommand = 'x'\n[landing_verification]\n",
                "dagq.toml:4: [landing_verification] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_landing_verification(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[landing_verification]\nreplaces = ['gate']\ncommand = 'landing'\n",
        )
        .unwrap();
        let verifier = ShellVerifier {
            checkout: dir.path().to_path_buf(),
            db: dir.path().join("queue.db"),
            user_config: None,
            verification_timeout: Duration::from_secs(1),
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier.landing_verification().unwrap().unwrap().command,
            "landing"
        );
        assert_eq!(verifier.ci_watch_config().unwrap(), None);
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

    /// `[throughput_review] kpi_stats` is a bool, on without it; a value
    /// that is not a bool, another key or the key twice is an error, and
    /// the table is not `[roles.throughput_review]`.
    #[test]
    fn parses_the_kpi_stats_of_the_throughput_review_table() {
        let read = |text: &str| parse_config(text).unwrap().throughput_review;
        assert_eq!(read(""), ThroughputReviewConfig::default());
        assert!(read("").kpi_stats);
        assert!(read("[throughput_review]\n").kpi_stats);
        assert!(read("[throughput_review]\nkpi_stats = true\n").kpi_stats);
        assert!(!read("[throughput_review]\nkpi_stats = false # heavy\n").kpi_stats);
        assert!(
            read("[roles.throughput_review]\nprovider = 'codex'\n").kpi_stats,
            "the role's table is another table"
        );
        for (text, expected) in [
            (
                "[throughput_review]\nkpi_stats = 0\n",
                "dagq.toml:2: value of kpi_stats: expected true or false",
            ),
            (
                "[throughput_review]\nkpi_stats = \"off\"\n",
                "dagq.toml:2: value of kpi_stats: expected true or false",
            ),
            (
                "[throughput_review]\nkpi = false\n",
                "dagq.toml:2: unknown key kpi in [throughput_review]; the keys are kpi_stats",
            ),
            (
                "[throughput_review]\nkpi_stats = false\nkpi_stats = true\n",
                "dagq.toml:3: kpi_stats is defined twice",
            ),
            (
                "[throughput_review]\n[throughput_review]\n",
                "[throughput_review] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_throughput_review(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[throughput_review]\nkpi_stats = false\n",
        )
        .unwrap();
        assert_eq!(
            load_throughput_review(dir.path()).unwrap(),
            Some(ThroughputReviewConfig { kpi_stats: false })
        );
    }

    /// `[eval]` and `[eval.providers.<provider>]` set the eval's limits
    /// and a provider's estimate and prices; an unset key keeps its
    /// default, and a key, a value or a provider out of range is an error.
    #[test]
    fn parses_the_evals_limits_and_its_providers_prices() {
        let eval = |text: &str| parse_config(text).map(|config| config.eval);
        assert_eq!(eval("").unwrap(), EvalConfig::default());
        let read = eval(
            "[eval]\nmax_runs = 60\nmax_cost_usd = 12.5\nconcurrency = 2\nthreshold = 0.8\nrecent_runs = 5\n\
             watch_new_reviews = 4\nwatch_interval_days = 3\n\
             [eval.providers.codex]\ninput_usd_per_mtok = 1.25\noutput_usd_per_mtok = 10\ndefault_run_usd = 0.3\n",
        )
        .unwrap();
        assert_eq!(
            (
                read.max_runs,
                read.max_cost_usd,
                read.concurrency,
                read.threshold,
                read.recent_runs
            ),
            (60, 12.5, 2, 0.8, 5)
        );
        assert_eq!((read.watch.new_reviews, read.watch.interval_days), (4, 3));
        assert!(read.watch.enabled);
        assert!(!eval("[eval]\nwatch = false\n").unwrap().watch.enabled);
        assert!(eval("[eval]\nwatch = true\n").unwrap().watch.enabled);
        let codex = read.provider(crate::domain::Provider::Codex);
        assert!(codex.converts());
        assert_eq!(codex.default_run_usd, Some(0.3));
        // Claude keeps its default estimate.
        assert_eq!(
            read.provider(crate::domain::Provider::Claude)
                .default_run_usd,
            Some(crate::domain::agent_eval::round::DEFAULT_CLAUDE_RUN_USD)
        );
        for wrong in [
            "[eval]\nthreshold = 1.5\n",
            "[eval]\nmax_runs = 0\n",
            "[eval]\nwatch_interval_days = 0\n",
            "[eval]\nwatch_new_reviews = -1\n",
            "[eval]\nwatch = 0\n",
            "[eval]\nwatch = \"off\"\n",
            "[eval]\nwatch = false\nwatch = true\n",
            "[eval]\nlimit = 3\n",
            "[eval]\nmax_runs = 3\nmax_runs = 4\n",
            "[eval.providers.gemini]\ndefault_run_usd = 1\n",
            "[eval.providers.codex]\nprice = 1\n",
            "[eval.providers.codex]\n[eval.providers.codex]\n",
        ] {
            assert!(eval(wrong).is_err(), "{wrong}");
        }
    }

    /// `[review.jobs]` sets each kind's timeout in whole seconds above
    /// 0 and the agent jobs at once in whole jobs above 0; a kind without
    /// its key keeps the provider's review timeout, and no `concurrency`
    /// keeps its default.
    #[test]
    fn parses_the_timeouts_and_the_concurrency_of_the_review_jobs_table() {
        let jobs = |text: &str| parse_config(text).unwrap().review_jobs;
        assert_eq!(jobs(""), ReviewJobs::default());
        assert_eq!(jobs("[review.jobs]\n"), ReviewJobs::default());
        assert_eq!(
            jobs("[review.jobs]\nprogram_timeout_secs = 120 # seconds\n"),
            ReviewJobs {
                agent: None,
                program: Some(120),
                concurrency: None,
            }
        );
        assert_eq!(
            jobs("[review.jobs]\nagent_timeout_secs = 1_800\nprogram_timeout_secs = 60\n"),
            ReviewJobs {
                agent: Some(1800),
                program: Some(60),
                concurrency: None,
            }
        );
        // How many agent jobs run at once, a whole number of jobs.
        let limited = jobs("[review.jobs]\nconcurrency = 4\n");
        assert_eq!((limited.concurrency, limited.concurrency()), (Some(4), 4));
        assert_eq!(jobs("").concurrency(), 10);
        // Beside `[review.subagents.<agent>]`, as its own table.
        assert_eq!(
            jobs("[review.subagents.docs]\npaths = [\"docs/**\"]\n[review.jobs]\nagent_timeout_secs = 5\n")
                .agent,
            Some(5)
        );
        for (text, error) in [
            (
                "[review.jobs]\nagent_timeout_secs = 0\n",
                "must be a positive",
            ),
            (
                "[review.jobs]\nagent_timeout_secs = \"10m\"\n",
                "expected a whole",
            ),
            (
                "[review.jobs]\ntimeout = 1\n",
                "dagq.toml:2: unknown key timeout in [review.jobs]; the keys are agent_timeout_secs, program_timeout_secs, concurrency",
            ),
            (
                "[review.jobs]\nconcurrency = 0\n",
                "must be a positive number of jobs",
            ),
            (
                "[review.jobs]\nagent_timeout_secs = 1\nagent_timeout_secs = 2\n",
                "dagq.toml:3: agent_timeout_secs is defined twice",
            ),
            (
                "[review.jobs]\n[review.jobs]\n",
                "dagq.toml:2: [review.jobs] is defined twice",
            ),
        ] {
            let got = format!("{:#}", parse_config(text).unwrap_err());
            assert!(got.contains(error), "{text:?}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_review_jobs(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[review.jobs]\nprogram_timeout_secs = 30\n",
        )
        .unwrap();
        assert_eq!(
            load_review_jobs(dir.path()).unwrap().unwrap().program,
            Some(30)
        );
    }

    /// `[retention]` sets `days` and `backups`, each a whole number above
    /// 0 taken once, and leaves a key it does not set at its default
    /// (ADR-t697-1).
    #[test]
    fn parses_the_retention_table() {
        let retention = |text: &str| parse_config(text).unwrap().retention;
        assert_eq!(retention(""), RetentionConfig::default());
        assert_eq!(
            retention("[retention]\ndays = 3 # days\n"),
            RetentionConfig {
                days: 3,
                ..RetentionConfig::default()
            }
        );
        assert_eq!(
            retention("[retention]\nbackups = 5\ndays = 14\n"),
            RetentionConfig {
                days: 14,
                backups: 5
            }
        );
        for (text, error) in [
            ("[retention]\ndays = 0\n", "dagq.toml:2: value of days"),
            (
                "[retention]\nbackups = -1\n",
                "dagq.toml:2: value of backups",
            ),
            (
                "[retention]\nhours = 1\n",
                "dagq.toml:2: unknown key hours in [retention]; the keys are days, backups",
            ),
            (
                "[retention]\ndays = 1\ndays = 2\n",
                "dagq.toml:3: days is defined twice",
            ),
        ] {
            let got = format!("{:#}", parse_config(text).unwrap_err());
            assert!(got.contains(error), "{text:?}: {got}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_retention_config(dir.path()).unwrap(),
            RetentionConfig::default()
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[retention]\nbackups = 1\n",
        )
        .unwrap();
        assert_eq!(load_retention_config(dir.path()).unwrap().backups, 1);
    }

    #[test]
    fn parses_the_landing_batch_cap_and_preserves_the_default() {
        let size = |text| {
            parse_config(text)
                .unwrap()
                .verification
                .landing_batch_size
                .get()
        };
        assert_eq!(size(""), 1);
        assert_eq!(size("[verification]\nlanding_batch_size = 2\n"), 2);
        assert!(parse_config("[verification]\nlanding_batch_size = 0\n").is_err());
        assert!(
            parse_config("[verification]\nlanding_batch_size = 2\nlanding_batch_size = 3\n")
                .is_err()
        );
    }

    /// `[verification] command_timeout_secs` is a whole number of seconds
    /// above 0 and at most a week, none without it; a value of another
    /// kind, an unknown key or a key set twice is an error of `dagq.toml`
    /// naming the line and the value. The verifier's limit is the declared
    /// one, else its own (the default, or what a test sets).
    #[test]
    fn parses_the_limit_of_a_verification_command() {
        let limit = |text: &str| parse_config(text).unwrap().verification;
        assert_eq!(limit(""), VerificationConfig::default());
        assert_eq!(limit("[verification]\n").command_timeout_secs, None);
        assert_eq!(
            limit("[verification]\ncommand_timeout_secs = 5_400 # 90 minutes\n")
                .command_timeout_secs,
            Some(5400)
        );
        for (text, expected) in [
            (
                "[verification]\ncommand_timeout_secs = 0\n",
                "dagq.toml:2: value of command_timeout_secs: must be a positive number of seconds, not 0",
            ),
            (
                "[verification]\ncommand_timeout_secs = -60\n",
                "dagq.toml:2: value of command_timeout_secs: must be a positive number of seconds, not -60",
            ),
            (
                "[verification]\ncommand_timeout_secs = 1.5\n",
                "dagq.toml:2: value of command_timeout_secs: expected a whole number of seconds, not 1.5",
            ),
            (
                "[verification]\ncommand_timeout_secs = \"1h\"\n",
                "dagq.toml:2: value of command_timeout_secs: expected a whole number of seconds, not \"1h\"",
            ),
            (
                "[verification]\ncommand_timeout_secs = 604801\n",
                "dagq.toml:2: value of command_timeout_secs: must be at most 604800 seconds (a week), not 604801",
            ),
            (
                "[verification]\ntimeout = 60\n",
                "dagq.toml:2: unknown key timeout in [verification]; the keys are command_timeout_secs",
            ),
            (
                "[verification]\ncommand_timeout_secs = 60\ncommand_timeout_secs = 70\n",
                "dagq.toml:3: command_timeout_secs is defined twice",
            ),
            (
                "[verification]\n[verification]\n",
                "dagq.toml:2: [verification] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        let verifier = ShellVerifier {
            checkout: dir.path().to_path_buf(),
            db: dir.path().join("queue.db"),
            user_config: None,
            verification_timeout: super::super::adapters::VERIFICATION_TIMEOUT,
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier.verification_timeout().unwrap(),
            Duration::from_secs(3600)
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[stall]\n").unwrap();
        assert_eq!(
            verifier.verification_timeout().unwrap(),
            Duration::from_secs(3600)
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[verification]\ncommand_timeout_secs = 5400\n",
        )
        .unwrap();
        assert_eq!(
            verifier.verification_timeout().unwrap(),
            Duration::from_secs(5400)
        );
        let short = ShellVerifier {
            verification_timeout: Duration::from_secs(1),
            ..verifier
        };
        assert_eq!(
            short.verification_timeout().unwrap(),
            Duration::from_secs(5400)
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[verification]\n").unwrap();
        assert_eq!(
            short.verification_timeout().unwrap(),
            Duration::from_secs(1)
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[verification]\ncommand_timeout_secs = 0\n",
        )
        .unwrap();
        let error = format!("{:#}", short.verification_timeout().unwrap_err());
        assert!(error.contains("value of command_timeout_secs"), "{error}");
    }

    /// `[fresh_session] peak_context_above` is a whole number of tokens
    /// above 0, none without it; the table knows no other key and takes
    /// the key once (ADR-t2080-1).
    #[test]
    fn parses_the_threshold_of_the_fresh_session_table() {
        let fresh = |text: &str| parse_config(text).unwrap().fresh_session;
        assert_eq!(fresh(""), FreshSessionConfig::default());
        assert_eq!(fresh("").peak_context_above, None);
        assert_eq!(fresh("[fresh_session]\n"), FreshSessionConfig::default());
        assert_eq!(
            fresh("[fresh_session]\npeak_context_above = 150_000 # tokens\n").peak_context_above,
            Some(150_000)
        );
        assert_eq!(
            fresh("[fresh_session]\npeak_context_above = 1\n").peak_context_above,
            Some(1)
        );
        for (text, expected) in [
            (
                "[fresh_session]\npeak_context_above = 0\n",
                "dagq.toml:2: value of peak_context_above: must be a positive number of tokens, not 0",
            ),
            (
                "[fresh_session]\npeak_context_above = -1\n",
                "dagq.toml:2: value of peak_context_above: must be a positive number of tokens, not -1",
            ),
            (
                "[fresh_session]\npeak_context_above = 1.5\n",
                "dagq.toml:2: value of peak_context_above: expected a whole number of tokens, not 1.5",
            ),
            (
                "[fresh_session]\npeak_context_above = \"150k\"\n",
                "dagq.toml:2: value of peak_context_above: expected a whole number of tokens",
            ),
            (
                "[fresh_session]\npeak_context_above = \n",
                "dagq.toml:2: value of peak_context_above: missing value",
            ),
            (
                "[fresh_session]\nthreshold = 1\n",
                "dagq.toml:2: unknown key threshold in [fresh_session]; the keys are peak_context_above",
            ),
            (
                "[fresh_session]\npeak_context_above = 1\npeak_context_above = 2\n",
                "dagq.toml:3: peak_context_above is defined twice",
            ),
            (
                "[fresh_session]\n[fresh_session]\n",
                "dagq.toml:2: [fresh_session] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_fresh_session(dir.path()).unwrap(), None);
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[stall]\n").unwrap();
        assert_eq!(
            load_fresh_session(dir.path()).unwrap(),
            Some(FreshSessionConfig::default())
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[fresh_session]\npeak_context_above = 200000\n",
        )
        .unwrap();
        assert_eq!(
            load_fresh_session(dir.path()).unwrap(),
            Some(FreshSessionConfig {
                peak_context_above: Some(200_000)
            })
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[fresh_session]\npeak_context_above = 0\n",
        )
        .unwrap();
        assert!(load_fresh_session(dir.path()).is_err());
    }

    /// `[replan]` turns the runtime's detection and diagnosis on with
    /// `enabled = true` and sets its limits, each a whole number above 0
    /// with its default where unset; without the table (or with `enabled =
    /// false`) it is off, the limits read as their defaults. The table
    /// knows no other key and takes each key once.
    #[test]
    fn parses_the_replan_table_and_leaves_it_off_without_it() {
        let replan = |text: &str| parse_config(text).unwrap().replan;
        let off = ReplanConfig::default();
        assert!(!off.enabled);
        assert_eq!(
            (
                off.non_pass_reviews,
                off.continue_reviews,
                off.max_auto_parks
            ),
            (3, 2, 3)
        );
        assert_eq!(replan(""), off);
        assert_eq!(
            replan(
                "[replan]
"
            ),
            off
        );
        assert_eq!(
            replan(
                "[replan]
enabled = false
"
            ),
            off
        );
        assert_eq!(
            replan(
                "[replan]
enabled = true # detect
"
            ),
            ReplanConfig {
                enabled: true,
                ..off
            }
        );
        assert_eq!(
            replan(
                "[replan]
enabled = true
non_pass_reviews = 4
continue_reviews = 1
max_auto_parks = 2
"
            ),
            ReplanConfig {
                enabled: true,
                non_pass_reviews: 4,
                continue_reviews: 1,
                max_auto_parks: 2,
            }
        );
        for (text, expected) in [
            (
                "[replan]
enabled = yes
",
                "dagq.toml:2: value of enabled: expected true or false, not yes",
            ),
            (
                "[replan]
non_pass_reviews = 0
",
                "dagq.toml:2: value of non_pass_reviews: must be a positive number of reviews, not 0",
            ),
            (
                "[replan]
continue_reviews = 1.5
",
                "dagq.toml:2: value of continue_reviews: expected a whole number of reviews, not 1.5",
            ),
            (
                "[replan]
max_auto_parks = -1
",
                "dagq.toml:2: value of max_auto_parks: must be a positive number of parks, not -1",
            ),
            (
                "[replan]
turns = 12
",
                "dagq.toml:2: unknown key turns in [replan]; the keys are enabled, non_pass_reviews, continue_reviews, max_auto_parks",
            ),
            (
                "[replan]
enabled = true
enabled = false
",
                "dagq.toml:3: enabled is defined twice",
            ),
            (
                "[replan]
[replan]
",
                "dagq.toml:2: [replan] is defined twice",
            ),
        ] {
            let error = format!("{:#}", parse_config(text).unwrap_err());
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_replan(dir.path()).unwrap(), None);
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[replan]
enabled = true
",
        )
        .unwrap();
        assert!(load_replan(dir.path()).unwrap().unwrap().enabled);
    }

    /// This repository's own `dagq.toml` reads, and turns the runtime's
    /// detection on with every other key of `[replan]` at its default.
    #[test]
    fn the_repositorys_own_config_turns_the_replan_detection_on_at_the_defaults() {
        let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(CONFIG_FILE_NAME))
            .unwrap();
        assert_eq!(
            parse_config(&text).unwrap().replan,
            ReplanConfig {
                enabled: true,
                ..ReplanConfig::default()
            }
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
    fn parses_the_admission_stall_limit_as_a_nonnegative_pass_count() {
        for (value, count) in [
            ("0", Some(0)),
            ("2", Some(2)),
            ("4294967295", Some(u32::MAX)),
        ] {
            let loaded =
                parse_config(&format!("[supervisor]\nadmission_stall_passes = {value}\n")).unwrap();
            assert_eq!(loaded.supervisor.admission_stall_passes, count);
        }
        for value in ["-1", "1.5", "4294967296"] {
            let error = parse_config(&format!("[supervisor]\nadmission_stall_passes = {value}\n"))
                .unwrap_err();
            assert!(format!("{error:#}").contains("value of admission_stall_passes"));
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
                admission_stall_passes: None,
                light_changes: None,
                max_spikes: None,
                class_aging: None,
                max_investigations: None,
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
                admission_stall_passes: None,
                light_changes: None,
                max_spikes: None,
                class_aging: None,
                max_investigations: None,
            }
        );
        assert_eq!(
            parse_config("").unwrap().supervisor,
            SupervisorConfig::default()
        );
        // The class limits (ADR-t1487-2).
        let classes = parse_config("[supervisor]\nmax_spikes = 1\nclass_aging = 1800\n")
            .unwrap()
            .supervisor;
        assert_eq!(
            (classes.max_spikes, classes.class_aging),
            (Some(1), Some(1800))
        );
        for (text, message) in [
            (
                "[supervisor]\nmax_spikes = 0\n",
                "dagq.toml:2: value of max_spikes",
            ),
            (
                "[supervisor]\nclass_aging = 0\n",
                "must be a positive number",
            ),
            (
                "[supervisor]\nclass_aging = 5000000000\n",
                "value of class_aging",
            ),
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
                "unknown key slots in [supervisor]; the keys are parallel, max_waiting, runtime_planners, claim_spacing, light_changes, max_spikes, class_aging",
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
                admission_stall_passes: None,
                light_changes: None,
                max_spikes: None,
                class_aging: None,
                max_investigations: None,
            })
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[supervisors]\n").unwrap();
        let error = format!("{:#}", load_supervisor_config(dir.path()).unwrap_err());
        assert!(
            error.contains(
                "[supervisor], [areas], [tasks], [goals], [e2e], [headless], [provider_fallback], [throughput_review], [review.jobs], [review], [review.code], [ci_watch], [landing_verification], [eval], [eval.providers.<provider>], [egress], [container.image], [container.build_output], [container.worker_cache], [container.verification], [telemetry], [[telemetry.commands]], [[recurring]] and [kpi]"
            ),
            "{error}"
        );
    }

    #[test]
    fn reads_the_recheck_table() {
        let config =
            parse_config("[recheck]\ncommand = \"cargo check --locked\" # fast\n").unwrap();
        assert_eq!(
            config.recheck.command.as_deref(),
            Some("cargo check --locked")
        );
        assert!(config.recheck.paths.is_empty());
        assert_eq!(parse_config("").unwrap().recheck, RecheckConfig::default());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_recheck_config(dir.path()).unwrap(),
            RecheckConfig::default()
        );
        fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[run.env]\nA = 'x'\n[recheck]\ncommand = 'make check'\npaths = [\"**/*.rs\", 'Cargo.lock', \"**/*.rs\"] # rust\n",
        )
        .unwrap();
        assert_eq!(
            load_recheck_config(dir.path()).unwrap(),
            RecheckConfig {
                command: Some("make check".to_owned()),
                paths: vec!["**/*.rs".to_owned(), "Cargo.lock".to_owned()],
            }
        );
        let verifier = ShellVerifier {
            checkout: dir.path().to_owned(),
            db: dir.path().join("queue.db"),
            user_config: None,
            verification_timeout: crate::infrastructure::adapters::VERIFICATION_TIMEOUT,
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
        };
        assert_eq!(
            verifier.recheck_config().unwrap().command.as_deref(),
            Some("make check")
        );
        fs::write(dir.path().join(CONFIG_FILE_NAME), "[recheck]\nnope = 1\n").unwrap();
        assert!(load_recheck_config(dir.path()).is_err());
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

    /// `[stall] resume_timeout_secs` sets how long the supervisor waits for
    /// a session it typed a request into; no key and no table keep the
    /// 3600 s default, and a value that is not a positive whole number of
    /// seconds stops the read as the other keys of the table do.
    #[test]
    fn loads_the_resume_timeout_of_the_stall_table() {
        let dir = tempfile::tempdir().unwrap();
        let load = |text: &str| {
            fs::write(dir.path().join(CONFIG_FILE_NAME), text).unwrap();
            load_stall_config(dir.path())
        };
        let set = load("[stall]\nresume_timeout_secs = 7200\n")
            .unwrap()
            .unwrap();
        assert_eq!(set.resume_timeout_secs, 7200);
        assert_eq!(set.resume_timeout(), Duration::from_secs(7200));
        for text in ["[stall]\nsend_confirm_secs = 30\n", "[supervisor]\n", ""] {
            let config = load(text).unwrap().unwrap();
            assert_eq!(config.resume_timeout_secs, 3600, "{text:?}");
        }
        for value in ["0", "-1", "1.5", "\"7200\""] {
            let error =
                load(&format!("[stall]\nresume_timeout_secs = {value}\n")).expect_err(value);
            assert!(
                format!("{error:#}").contains("resume_timeout_secs"),
                "{value}: {error:#}"
            );
        }
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
            profiles: &crate::domain::toolchain::BUILTIN,
            children: crate::domain::environment::RunChildren::default(),
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

#[cfg(test)]
mod landing_approval_tests {
    use super::*;
    use crate::{
        application::supervise::repository_config::{
            AppToken, ConfigUnavailable, InstallationTokens, MainRules,
        },
        domain::landing_approval::{ApprovalMatch, matching_conditions},
    };

    struct MainFile<'a> {
        text: &'a str,
        fail: Option<&'static str>,
    }
    impl InstallationTokens for MainFile<'_> {
        fn issue(
            &self,
            scope: &serde_json::Value,
            _: i64,
        ) -> std::result::Result<AppToken, ConfigUnavailable> {
            assert_eq!(
                scope,
                &crate::application::supervise::repository_config::read_scope("repo")
            );
            if self.fail == Some("token") {
                return Err(ConfigUnavailable);
            }
            Ok(AppToken("test".into()))
        }
    }
    impl MainRules for MainFile<'_> {
        fn main_sha(&self, _: &AppToken) -> std::result::Result<String, ConfigUnavailable> {
            if self.fail == Some("sha") {
                return Err(ConfigUnavailable);
            }
            Ok("trusted-main-sha".into())
        }
        fn contents(
            &self,
            _: &AppToken,
            sha: &str,
        ) -> std::result::Result<String, ConfigUnavailable> {
            assert_eq!(sha, "trusted-main-sha");
            if self.fail == Some("contents") {
                return Err(ConfigUnavailable);
            }
            Ok(self.text.into())
        }
    }

    #[test]
    fn main_conditions_survive_run_removal_or_relaxation() {
        let main = MainFile {
            text: "[landing.approval]\npaths = ['protected/**']\ntwo_person = true",
            fail: None,
        };
        let trusted = read_main_approval_conditions(&main, "repo", 0).unwrap();
        assert_eq!(trusted.main_commit(), "trusted-main-sha");
        assert!(trusted.two_person());
        for run_text in [
            "",
            "[landing.approval]\npaths = []\ntwo_person = false",
            "[landing.approval]\npaths = ['other/**']",
        ] {
            let _run = parse_config(run_text).unwrap();
            assert_eq!(
                matching_conditions(
                    &trusted,
                    &["dagq.toml".into(), "protected/edit".into()],
                    None,
                    &[]
                ),
                vec![ApprovalMatch::Path("protected/**".into())]
            );
        }
    }

    #[test]
    fn invalid_main_blocks_landing_and_absent_section_preserves_defaults() {
        assert_eq!(parse_config("").unwrap(), Config::default());
        for text in [
            "",
            "[landing.approval]",
            "[landing.approval]\ntwo_person = false",
            "[landing.approval]\ntwo_person = true",
        ] {
            let main = MainFile { text, fail: None };
            let trusted = read_main_approval_conditions(&main, "repo", 0).unwrap();
            assert_eq!(trusted.two_person(), text.ends_with("true"));
            assert!(
                matching_conditions(
                    &trusted,
                    &["any/path".into()],
                    Some(crate::domain::AskConfidence::Low),
                    &[]
                )
                .is_empty()
            );
        }
        for entry in [
            "unknown = []",
            "review_confidence = ['high']",
            "review_confidence = ['medium']",
            "review_confidence = 'low'",
            "paths = true",
            "paths = ['']",
            "paths = ['a//b']",
            "goal_marks = [' ']",
            "goal_marks = false",
            "two_person = 'true'",
            "two_person = 1",
            "paths = ['/absolute']",
            "paths = ['a/../b']",
            "paths = []\npaths = []",
        ] {
            let text = format!("[landing.approval]\n{entry}");
            assert!(parse_config(&text).is_err(), "{entry}");
            let main = MainFile {
                text: &text,
                fail: None,
            };
            assert_eq!(
                read_main_approval_conditions(&main, "repo", 0),
                Err(LandingConfigError),
                "{entry}"
            );
        }
        let invalid = MainFile {
            text: "[landing.approval]\nreview_confidence = ['unknown']",
            fail: None,
        };
        assert_eq!(
            read_main_approval_conditions(&invalid, "repo", 0),
            Err(LandingConfigError)
        );
        for fail in ["token", "sha", "contents"] {
            let main = MainFile {
                text: "",
                fail: Some(fail),
            };
            assert_eq!(
                read_main_approval_conditions(&main, "repo", 0),
                Err(LandingConfigError)
            );
        }
        assert!(LandingConfigError.to_string().contains("landing blocked"));
    }

    #[test]
    fn all_declared_conditions_are_interpreted() {
        use crate::domain::{AskConfidence, landing_approval::GoalMark};
        let main = MainFile {
            text: "[landing.approval]\npaths = ['docs/**']\nreview_confidence = ['low']\ngoal_marks = ['sensitive']",
            fail: None,
        };
        let trusted = read_main_approval_conditions(&main, "repo", 0).unwrap();
        let mark = GoalMark::new("sensitive".into()).unwrap();
        assert_eq!(mark.as_str(), "sensitive");
        assert_eq!(
            matching_conditions(
                &trusted,
                &["docs/change".into()],
                Some(AskConfidence::Low),
                std::slice::from_ref(&mark)
            ),
            vec![
                ApprovalMatch::Path("docs/**".into()),
                ApprovalMatch::LowConfidence,
                ApprovalMatch::GoalMark(mark)
            ]
        );
        assert!(!trusted.two_person());
    }
}
