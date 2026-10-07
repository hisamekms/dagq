//! Codex CLI as a headless worker (ADR-t813-1, ADR-t813-3): each turn is
//! one `codex exec --json` (the first) or `codex exec resume --json
//! <thread id>` (every later one) in the run's worktree, in the
//! workspace-write sandbox with the writable roots, network and approval
//! policy of ADR-t813-3 given as `-c` on every call (a resume does not
//! keep the first call's), and the project rules that forbid `pkill` and
//! `killall` in the worktree. Nothing of the person's (`~/.codex`,
//! `CODEX_HOME`) is written or changed: the model a turn ran on is only
//! read from the thread's rollout there.
//!
//! Codex also runs the headless jobs whose role `[roles.<role>]` puts on it
//! (ADR-t1063-1, the goal review for now): one `codex exec --json` in the
//! read-only sandbox, whose reply, thread and model are read from its JSONL
//! and its rollout the way a turn's are.

use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::application::{AgentProvider, CommandSpec, RUN_TMP_DIR, TurnReader, TurnTarget};
use crate::domain::{
    TaskRun,
    actor_model::ActorLaunch,
    headless_job::{JobAccess, JobFailure, JobSession},
    review_subagents::{AgentTool, AgentTools},
    turn::{TurnFailure, TurnSession},
};

use super::{
    adapters::{output, provider_executable_on, relocated},
    codex_turns::CodexTurnReader,
};

/// Codex CLI (`codex`, resolved to its executable by [`executable`]:
/// cmux's shim would add hooks and trust flags of its own).
#[derive(Debug, Clone)]
pub struct Codex {
    pub executable: PathBuf,
    /// Codex's home, whose `sessions` hold the rollouts the model of a turn
    /// is read from; `None` for Codex's own (`$CODEX_HOME`, else
    /// `~/.codex`).
    pub home: Option<PathBuf>,
}

impl Codex {
    /// Codex at `executable`, with its own home.
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            home: None,
        }
    }

    /// The `sessions` directory of Codex's home; `None` when neither
    /// `CODEX_HOME` nor `HOME` is set.
    pub fn sessions_dir(&self) -> Option<PathBuf> {
        let home = self.home.clone().or_else(|| {
            std::env::var_os("CODEX_HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .filter(|home| !home.is_empty())
                        .map(|home| PathBuf::from(home).join(".codex"))
                })
        })?;
        Some(home.join("sessions"))
    }
}

/// The directories of cmux's per-surface shims (`$TMPDIR/cmux-cli-shims/
/// <surface>/codex`), which a cmux terminal puts first on PATH: the shim
/// adds cmux's hooks with `--dangerously-bypass-hook-trust`, and goes with
/// its surface.
const CMUX_SHIMS: &str = "cmux-cli-shims";

/// The name Codex is found again by (ADR-t2079-1).
pub const CODEX_NAME: &str = "codex";

/// `path` resolved to the Codex executable (ADR-t813-3 decision 6,
/// ADR-t2079-1): a bare name is looked for on `search` (PATH) past cmux's
/// shims, a path is taken as it is, and a symbolic link is kept, as
/// [`provider_executable_on`] does for every provider.
pub fn executable_on(path: &Path, search: &std::ffi::OsStr) -> Result<PathBuf> {
    provider_executable_on(path, search, |dir| {
        dir.to_string_lossy().contains(CMUX_SHIMS)
    })
    .map_err(|error| {
        if path.components().count() > 1 || path.is_absolute() {
            error
        } else {
            anyhow::anyhow!(
                "{} was not found on PATH (outside cmux's shims)",
                path.display()
            )
        }
    })
}

/// [`executable_on`] this process's PATH.
pub fn executable(path: &Path) -> Result<PathBuf> {
    executable_on(path, &std::env::var_os("PATH").unwrap_or_default())
}

/// Codex at `path` as [`provider_at_entry`](super::adapters::provider_at_entry)
/// resolves it.
pub fn codex_at_entry(path: &Path) -> Result<PathBuf> {
    super::adapters::provider_at_entry(path, CODEX_NAME, executable)
}

/// The project rules the runtime puts in a Codex run's worktree (ADR-t813-3
/// decision 5): `pkill` and `killall` are forbidden, with why. The sandbox
/// is the main defence (a process outside it can be neither listed nor
/// signalled); the rules only answer a plain `pkill` with the reason, as
/// `sh -c 'pkill …'` goes round them.
pub const RULES_PATH: &str = ".codex/rules/dagq-deny.rules";

/// The content of [`RULES_PATH`].
pub const RULES: &str = r#"# Written by dagq for this run's Codex worker (ADR-t813-3); not committed.
prefix_rule(pattern=["pkill"], decision="forbidden", justification="dagq: stop only processes you started, by pid or task; pkill also stops other runs' sessions and checks")
prefix_rule(pattern=["killall"], decision="forbidden", justification="dagq: stop only processes you started, by pid or task; killall also stops other runs' sessions and checks")
"#;

/// The sandbox of every turn, before its writable roots: workspace-write
/// (the worktree, `/tmp` and `$TMPDIR`), no approval (what needs one
/// fails at once instead of waiting) and the network open for sccache's
/// server and the registry (ADR-t813-3 decisions 1 and 4).
pub const SANDBOX_CONFIG: [&str; 3] = [
    r#"sandbox_mode="workspace-write""#,
    r#"approval_policy="never""#,
    "sandbox_workspace_write.network_access=true",
];

/// The places a Codex worker may write besides its worktree (ADR-t813-3
/// decision 2): of the Git common directory `G` only the worktree's own
/// directory, the objects and the refs (and their logs) of the `dagq/`
/// run branches, so that the run branch takes commits and `main` does
/// not; the run directory (the receipt); and cargo's registry.
pub fn writable_roots(worktree: &Path, run_dir: &Path, cargo_home: &Path) -> Result<Vec<PathBuf>> {
    let admin = worktree_admin_dir(worktree)?;
    let common = common_dir(&admin)?;
    Ok(vec![
        admin,
        common.join("objects"),
        common.join("refs/heads/dagq"),
        common.join("logs/refs/heads/dagq"),
        run_dir.to_owned(),
        cargo_home.join("registry"),
    ])
}

/// The directory Git keeps a linked worktree's state in: the `gitdir:` of
/// its `.git` file.
fn worktree_admin_dir(worktree: &Path) -> Result<PathBuf> {
    let file = worktree.join(".git");
    let text = fs::read_to_string(&file)
        .with_context(|| format!("{} is not a linked worktree's", file.display()))?;
    let dir = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .with_context(|| format!("{} names no gitdir", file.display()))?;
    Ok(worktree.join(dir))
}

/// The common directory of the worktree whose own directory is `admin`:
/// its `commondir` file, relative to `admin`.
fn common_dir(admin: &Path) -> Result<PathBuf> {
    let file = admin.join("commondir");
    let text = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let common = admin.join(text.trim());
    common
        .canonicalize()
        .with_context(|| format!("resolve {}", common.display()))
}

/// The `$TMPDIR` the turn would have had (the session wrapper's, `value`)
/// as a writable root, since the turn's own `TMPDIR` is the run
/// directory's [`RUN_TMP_DIR`] (task 1290): workspace-write lets the
/// sandbox write the `$TMPDIR` of Codex's own environment, which is no
/// longer that one, and ADR-t813-3 decision 2 keeps it writable. `None`
/// when unset, empty or relative; resolved when it can be (the sandbox
/// matches real paths: `/var` is `/private/var` on macOS).
pub fn starter_tmpdir(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let dir = PathBuf::from(value.filter(|value| !value.is_empty())?);
    if !dir.is_absolute() {
        return None;
    }
    Some(dir.canonicalize().unwrap_or(dir))
}

/// The run directory's [`RUN_TMP_DIR`], made a real directory for a turn's
/// `TMPDIR` (task 1290). What the worker may have put there in its place
/// (a link, a file: the run directory is a writable root) is removed, not
/// followed, so that the `TMPDIR` the sandbox lets the turn write is never
/// a directory outside the run directory.
pub fn run_tmp_dir(run_dir: &Path) -> Result<PathBuf> {
    let tmp = run_dir.join(RUN_TMP_DIR);
    match fs::symlink_metadata(&tmp) {
        Ok(metadata) if metadata.is_dir() => return Ok(tmp),
        Ok(_) => fs::remove_file(&tmp).with_context(|| format!("remove {}", tmp.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", tmp.display())),
    }
    fs::create_dir_all(run_dir).with_context(|| format!("create {}", run_dir.display()))?;
    match fs::create_dir(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", tmp.display())),
    }
    if !fs::symlink_metadata(&tmp).is_ok_and(|metadata| metadata.is_dir()) {
        bail!("{} is not a directory", tmp.display());
    }
    Ok(tmp)
}

/// Cargo's home: `$CARGO_HOME`, else `~/.cargo`.
fn cargo_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME").filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cargo"))
}

/// `projects={"<worktree>"={trust_level="trusted"}}`: the worktree trusted
/// for the turn only. `codex exec` (0.155.1) trusts the project it starts
/// a thread in when no trust is configured for it and the sandbox may
/// write there, and persists that as `[projects."<main checkout>"]` in the
/// person's `~/.codex/config.toml`
/// (`app-server/src/request_processors/thread_processor.rs`
/// `thread_start_task`); a trust given here is found first
/// (`config/src/config_toml.rs` `get_active_project`), so nothing is
/// written. The path goes in the value, as a quoted key: the key of a
/// `-c` is split at every `.` (task 1174).
pub fn trust_config(worktree: &Path) -> Result<String> {
    project_trust(worktree, "trusted")
}

/// `projects={"<worktree>"={trust_level="untrusted"}}`: the worktree
/// untrusted for a run's review (ADR-t1570-1 decision 1). A Git worktree
/// of a main checkout the person's `~/.codex/config.toml` trusts takes
/// that trust, and Codex then loads the project's layer from the worktree,
/// which its worker can write: `.codex/config.toml` (its
/// `developer_instructions` reached the review, seen with codex-cli
/// 0.160.0) and `.codex/rules`. A trust given for the worktree itself is
/// found before the main checkout's, and `untrusted` loads neither; the
/// read-only sandbox, the job's permission profile and its commands run
/// as before, and no approval is waited for. Codex then no longer reads
/// the worktree's `AGENTS.md` by itself; the review's prompt names it
/// (ADR-t1470-1 decision 2).
pub fn distrust_config(worktree: &Path) -> Result<String> {
    project_trust(worktree, "untrusted")
}

/// `projects={"<worktree>"={trust_level="<level>"}}`.
fn project_trust(worktree: &Path, level: &str) -> Result<String> {
    let path = worktree
        .to_str()
        .with_context(|| format!("{} is not UTF-8", worktree.display()))?;
    Ok(format!(
        r#"projects={{{}={{trust_level="{level}"}}}}"#,
        serde_json::to_string(path)?
    ))
}

/// `-c sandbox_workspace_write.writable_roots=[…]`: the roots as a TOML
/// array of strings (JSON's escapes are TOML's).
pub fn writable_roots_config(roots: &[PathBuf]) -> Result<String> {
    let roots = roots
        .iter()
        .map(|root| {
            let text = root
                .to_str()
                .with_context(|| format!("{} is not UTF-8", root.display()))?;
            Ok(serde_json::to_string(text)?)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "sandbox_workspace_write.writable_roots=[{}]",
        roots.join(",")
    ))
}

/// The sandbox of a headless job (ADR-t1063-1 decision 2): read-only,
/// whatever its [`JobAccess`] says. Every intent a job names reads files
/// and runs `dagq`'s reads at most; the read-only sandbox lets any command
/// read and refuses every write, and `dagq` takes the role and queue from
/// the job's environment, which the sandbox's shell inherits (spike 1. and
/// 2.). No writable root, network, approval or bypass flag is given.
pub const JOB_SANDBOX: &str = "read-only";

/// The permission profile a headless job runs in when it reaches the
/// queue service (ADR-t1233-5 decision 4): Codex's `:read-only`, with the
/// network on only through Codex's network proxy, which lets the job reach
/// the service's socket and nothing else ([`job_service_config`]).
pub const JOB_PROFILE: &str = "dagq_job";

/// The `-c` of a headless job in place of `--sandbox read-only` that let
/// its `dagq` reach the queue service at `socket` (ADR-t1233-5 decision
/// 4): the [`JOB_PROFILE`] permission profile extends `:read-only` (no
/// write anywhere), turns its network on, and allows the one unix socket;
/// the `network_proxy` feature routes the network through Codex's proxy,
/// whose domain allowlist is empty, so no TCP connection leaves. Checked
/// with `codex sandbox` 0.159.2: the socket answers, a TCP connection and
/// a write are refused, and without the socket's entry the socket is
/// refused too.
pub fn job_service_config(socket: &Path) -> Result<Vec<String>> {
    // The sandbox matches the real path (`/tmp` is `/private/tmp`); a
    // socket not made yet is named under its real directory.
    let real = socket.canonicalize().or_else(|_| {
        let dir = socket.parent().context("a socket has a directory")?;
        let name = socket.file_name().context("a socket has a name")?;
        Ok::<_, anyhow::Error>(dir.canonicalize()?.join(name))
    });
    let real = real.unwrap_or_else(|_| socket.to_owned());
    let socket = real
        .to_str()
        .with_context(|| format!("{} is not UTF-8", real.display()))?;
    Ok(vec![
        "features.network_proxy=true".to_owned(),
        format!(r#"default_permissions="{JOB_PROFILE}""#),
        format!(r#"permissions.{JOB_PROFILE}.extends=":read-only""#),
        format!("permissions.{JOB_PROFILE}.network.enabled=true"),
        format!(
            r#"permissions.{JOB_PROFILE}.network.unix_sockets={{{}="allow"}}"#,
            serde_json::to_string(socket)?
        ),
    ])
}

/// The features of Codex's tools that run a command (codex-cli 0.160.0's
/// `[features]`): the shell tool and the unified exec tool.
pub const SHELL_FEATURES: [&str; 2] = ["shell_tool", "unified_exec"];

/// The `-c` of the launch of an agent's own job that give it the declared
/// `tools` (ADR-t1728-2 decision 3, ADR-t1895-1's agent job). Codex reads
/// files, searches and finds paths only by running commands, which the
/// job's [`JOB_SANDBOX`] (or [`JOB_PROFILE`]) keeps to reads: a job that
/// declares a read (`read`, `grep`, `glob`) or `shell` keeps its shell and
/// is given nothing, and one that declares none of them has the shell
/// turned off ([`SHELL_FEATURES`] `=false`). These only add features off:
/// the read-only sandbox, the permission profile and the run's review's
/// [`distrust_config`] (ADR-t1570-1) stay as its launch gives them, so a
/// declared `edit` or `write` writes nothing (no role allows one yet). No
/// launch is given them yet: the eval's agent job (task 1869) and the
/// run's review's (task 1903) will be.
pub fn agent_job_tools_config(tools: &AgentTools) -> Vec<String> {
    let runs_commands = [
        AgentTool::Read,
        AgentTool::Grep,
        AgentTool::Glob,
        AgentTool::Shell,
    ]
    .into_iter()
    .any(|tool| tools.has(tool));
    if runs_commands {
        return Vec::new();
    }
    SHELL_FEATURES
        .iter()
        .map(|feature| format!("features.{feature}=false"))
        .collect()
}

/// The last `agent_message` of a `codex exec --json` output, in full: the
/// reply of a headless job; empty when there is none.
pub fn last_message(stdout: &str) -> String {
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "item.completed" && event["item"]["type"] == "agent_message")
        .and_then(|event| event["item"]["text"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Codex's reasoning effort for the effort a claim chose: Codex has no
/// `max`, its highest is `xhigh`.
pub fn reasoning_effort(effort: &str) -> &str {
    match effort {
        "max" => "xhigh",
        other => other,
    }
}

/// Write [`RULES`] to the worktree, and keep it out of Git through the
/// repository's `info/exclude` (shared by its worktrees; the line is added
/// once), so that the worktree stays clean for the receipt.
pub fn write_rules(worktree: &Path) -> Result<()> {
    let path = worktree.join(RULES_PATH);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(&path, RULES).with_context(|| format!("write {}", path.display()))?;
    let exclude = common_dir(&worktree_admin_dir(worktree)?)?.join("info/exclude");
    let line = format!("/{RULES_PATH}");
    let text = match fs::read_to_string(&exclude) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", exclude.display())),
    };
    if !text.lines().any(|existing| existing.trim() == line) {
        if let Some(dir) = exclude.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let separator = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        // Appended, never rewritten: the file is the repository's, read by
        // Git and by other runs' turns meanwhile. Two turns that append at
        // once leave the line twice, which Git takes as once.
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude)
            .and_then(|mut file| file.write_all(format!("{separator}{line}\n").as_bytes()))
            .with_context(|| format!("append to {}", exclude.display()))?;
    }
    Ok(())
}

impl AgentProvider for Codex {
    fn preflight(&self) -> Result<()> {
        output(Command::new(&self.executable).arg("--version"))?;
        Ok(())
    }
    fn relocated_executable(&self) -> Option<PathBuf> {
        relocated(&self.executable, CODEX_NAME, executable)
    }
    fn command(&self, _: &TaskRun, _: &str) -> Result<CommandSpec> {
        bail!("Codex runs headless only: it has no interactive session")
    }
    fn resume_command(&self, _: &TaskRun) -> Result<CommandSpec> {
        bail!("Codex runs headless only: it has no interactive session")
    }
    /// Codex's sub-agents are not known to run in `exec`, to keep its
    /// sandbox or to ignore the worktree's `.codex` (ADR-t1453-1 decision
    /// 8): a review that requires subagents is not started on Codex, and
    /// one that requires none is as before.
    fn runs_review_subagents(&self) -> bool {
        false
    }
    /// A [`headless_command`](AgentProvider::headless_command) in the
    /// run's worktree with [`distrust_config`] as `-c`: the worker's
    /// `.codex` does not reach its review (ADR-t1570-1).
    fn review_command(
        &self,
        run: &TaskRun,
        prompt: &str,
        access: JobAccess,
    ) -> Result<CommandSpec> {
        let worktree = Path::new(run.worktree_path().context("missing worktree")?);
        let mut command = self.headless_command(worktree, prompt, access)?;
        command.arg("-c").arg(distrust_config(worktree)?);
        Ok(command)
    }
    /// `-m <model>` for a model of Codex's (a claim's Claude model is left
    /// to Codex's default) and `-c model_reasoning_effort="<effort>"`.
    fn select_model(&self, command: &mut CommandSpec, model: &str, effort: &str) {
        if !model.starts_with("claude") {
            command.option_args(["-m", model]);
        }
        command.option_args([
            "-c".to_owned(),
            format!(r#"model_reasoning_effort="{}""#, reasoning_effort(effort)),
        ]);
    }
    /// `codex exec --json -C <worktree>` for the first turn and `codex exec
    /// resume --json … <thread id>` after it (`resume` has no `-C`, so the
    /// worktree is its working directory either way), with
    /// [`SANDBOX_CONFIG`], the run's [`writable_roots`] and the worktree's
    /// [`trust_config`] as `-c` on each, the prompt after `--`, and
    /// `TMPDIR` set to the run directory's [`RUN_TMP_DIR`] (made if
    /// missing). It leads a session of its own, but Codex runs
    /// each command in a process group of the command's own, which a
    /// signal to the turn's group does not reach (task 1061): the wrapper
    /// stops a turn with its descendants by pid as well (task 1085).
    fn turn_command(
        &self,
        target: &TurnTarget<'_>,
        prompt: &str,
        session: TurnSession<'_>,
    ) -> Result<CommandSpec> {
        // Codex is given no broker tools yet: a `required` run's turn is
        // refused rather than run with its own (ADR-t838-1).
        if target.broker_required.is_some() {
            return Err(crate::application::broker_run::BrokerRequiredRefused(
                "[broker] mode = \"required\": a Codex worker is given no broker tools yet, and is \
not started with its own"
                    .to_owned(),
            )
            .into());
        }
        // Codex names a new thread itself.
        let resume = session.resumed();
        let (worktree, run_dir) = (target.cwd, target.dir);
        let mut roots = writable_roots(worktree, run_dir, &cargo_home()?)?;
        roots.extend(starter_tmpdir(std::env::var_os("TMPDIR")));
        write_rules(worktree)?;
        // The turn's temporary files go under the run directory, a writable
        // root, where the cleanup finds them once the task is over (task
        // 1290); `/tmp` and the starter's `$TMPDIR` stay writable as the
        // sandbox has them.
        let tmp = run_tmp_dir(run_dir)?;
        let mut command = CommandSpec::new(&self.executable);
        command.env("TMPDIR", &tmp);
        // Its `dagq ask` goes to the queue service in client mode, whose
        // socket the open network of the sandbox reaches (ADR-t1233-5
        // decisions 4 and 5): no request in the run directory.
        command.current_dir(worktree);
        command.arg("exec");
        if resume.is_some() {
            command.arg("resume");
        }
        command.arg("--json");
        if resume.is_none() {
            command.arg("-C").arg(worktree);
        }
        for config in SANDBOX_CONFIG
            .into_iter()
            .map(str::to_owned)
            .chain([writable_roots_config(&roots)?, trust_config(worktree)?])
        {
            command.arg("-c").arg(config);
        }
        command.arg("--");
        if let Some(thread) = resume {
            command.arg(thread);
        }
        command.arg(prompt).new_session();
        Ok(command)
    }
    fn turn_reader(&self) -> Result<Box<dyn TurnReader>> {
        Ok(Box::new(CodexTurnReader::reading(self.sessions_dir())))
    }
    /// `codex exec --json --skip-git-repo-check --sandbox read-only -C <cwd>`
    /// in `cwd` with the prompt as its standard input, never an argument
    /// (`exec` with no prompt reads it there, so a prompt of any size
    /// starts, task 1560) (ADR-t1063-1 decision 2, spike 1.): the job's
    /// intent is read in [`JOB_SANDBOX`], its environment (role, queue) is
    /// the caller's, and nothing of the person's Codex settings is changed.
    /// `--skip-git-repo-check` because the cwd of the throughput review,
    /// the observer and the recovery job is a job or run directory outside
    /// any Git repository, which `codex exec` refuses without it (task
    /// 1378); it only skips that check. No trust is given: the read-only
    /// sandbox cannot write the cwd, so Codex persists no trust for the
    /// project, and the job takes the trust the person's
    /// `~/.codex/config.toml` gives it. The goal review and the plan
    /// review run in the main checkout, whose project layer is landed,
    /// committed content the person trusts; the other jobs run outside any
    /// Git repository. Only a run's review, in a worktree its worker can
    /// write, is given [`distrust_config`] (by `review_command`,
    /// ADR-t1570-1 decision 2). Codex names its thread itself, so no
    /// session id is given.
    fn headless_command(&self, cwd: &Path, prompt: &str, access: JobAccess) -> Result<CommandSpec> {
        let _ = access;
        let mut command = CommandSpec::new(&self.executable);
        command
            .current_dir(cwd)
            .arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--sandbox")
            .arg(JOB_SANDBOX)
            .arg("-C")
            .arg(cwd)
            .stdin(prompt);
        Ok(command)
    }
    /// A job's model only when it is Codex's (`-m`), and its effort as
    /// `-c model_reasoning_effort` whenever the launch gives one: a Codex
    /// launch may leave the model to Codex's default.
    fn apply_launch(&self, command: &mut CommandSpec, launch: &ActorLaunch) {
        if let Some(model) = launch
            .model
            .as_deref()
            .filter(|model| !model.starts_with("claude"))
        {
            command.option_args(["-m", model]);
        }
        if let Some(effort) = launch.effort.as_deref() {
            command.option_args([
                "-c".to_owned(),
                format!(r#"model_reasoning_effort="{}""#, reasoning_effort(effort)),
            ]);
        }
    }
    fn job_reply(&self, stdout: &str) -> String {
        last_message(stdout)
    }
    /// A job ([`JOB_SANDBOX`]) runs in [`job_service_config`]'s profile
    /// instead, which reaches the socket and keeps the rest read-only and
    /// offline. A worker's turn needs nothing: its sandbox's network is
    /// open (ADR-t813-3 decision 4), and the socket is reached through it.
    fn reach_queue_service(&self, command: &mut CommandSpec, socket: &Path) {
        if !command.remove_arg_pair("--sandbox", JOB_SANDBOX) {
            return;
        }
        match job_service_config(socket) {
            Ok(configs) => {
                for config in configs {
                    command.option_args(["-c".to_owned(), config]);
                }
            }
            Err(error) => {
                // The job keeps the read-only sandbox, and its `dagq`
                // reports the service unreachable.
                tracing::warn!("a Codex job cannot be let reach the queue service: {error:#}");
                command.option_args(["--sandbox", JOB_SANDBOX]);
            }
        }
    }
    /// The thread `thread.started` names, and the model of the rollout's
    /// `turn_context` written since the job started.
    fn job_session(&self, stdout: &str, since: Option<i64>) -> Option<JobSession> {
        let result = read_job(self.sessions_dir(), stdout, "", since);
        Some(JobSession {
            session_id: result.session_id,
            model: result.model,
            model_unknown: result.model_unknown,
        })
    }
    /// The failure of the turn the job was, as a worker's turn is read
    /// ([`crate::infrastructure::codex_turns::classify`]).
    fn job_failure(&self, stdout: &str, stderr: &str) -> JobFailure {
        match read_job(None, stdout, stderr, None).failure {
            Some(TurnFailure::Authentication) => JobFailure::Authentication,
            Some(TurnFailure::UsageLimit) => JobFailure::UsageLimit,
            Some(TurnFailure::Launch) => JobFailure::LaunchFailed,
            _ => JobFailure::Other,
        }
    }
    fn turn_session_from_output(&self) -> bool {
        true
    }
}

/// A job's whole output read as one turn: its lines, then its end (read
/// as a process that failed, which only a failure is asked of).
fn read_job(
    sessions: Option<PathBuf>,
    stdout: &str,
    stderr: &str,
    since: Option<i64>,
) -> crate::domain::turn::TurnResult {
    let mut reader = CodexTurnReader::reading_since(sessions, since);
    for line in stdout.lines() {
        reader.line(line);
    }
    reader.finish(None, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository `G` with a linked worktree `wt` named `run`, as Git
    /// lays it out.
    fn layout(dir: &Path) -> (PathBuf, PathBuf) {
        let common = dir.join("repo/.git");
        let admin = common.join("worktrees/run");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(common.join("info")).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let worktree = dir.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        (common.canonicalize().unwrap(), worktree)
    }

    #[test]
    fn the_writable_roots_are_the_run_branch_the_run_directory_and_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        let roots = writable_roots(&worktree, Path::new("/runs/r1"), Path::new("/c")).unwrap();
        assert_eq!(
            roots,
            [
                dir.path().join("repo/.git/worktrees/run"),
                common.join("objects"),
                common.join("refs/heads/dagq"),
                common.join("logs/refs/heads/dagq"),
                PathBuf::from("/runs/r1"),
                PathBuf::from("/c/registry"),
            ]
        );
        assert_eq!(
            writable_roots_config(&[PathBuf::from("/a b"), PathBuf::from("/q\"x")]).unwrap(),
            r#"sandbox_workspace_write.writable_roots=["/a b","/q\"x"]"#
        );
        // Not a linked worktree: no roots to guess.
        assert!(writable_roots(dir.path(), Path::new("/r"), Path::new("/c")).is_err());
    }

    /// An executable stub at `path`.
    fn stub(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn codex_is_resolved_past_cmux_shims() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("cmux-cli-shims/s1");
        let bin = dir.path().join("bin");
        for d in [&shim, &bin] {
            stub(&d.join("codex"));
        }
        let search = std::env::join_paths([&shim, &bin]).unwrap();
        assert_eq!(
            executable_on(Path::new("codex"), &search).unwrap(),
            bin.join("codex")
        );
        let only_shim = std::env::join_paths([&shim]).unwrap();
        let error = executable_on(Path::new("codex"), &only_shim).unwrap_err();
        assert!(
            format!("{error:#}").contains("outside cmux's shims"),
            "{error:#}"
        );
        // A path is taken as it is.
        assert_eq!(
            executable_on(&shim.join("codex"), &search).unwrap(),
            shim.join("codex")
        );
        // A file that cannot be executed is no executable.
        fs::write(bin.join("plain"), "").unwrap();
        assert!(executable_on(&bin.join("plain"), &search).is_err());
    }

    /// ADR-t2079-1: Codex is started by its link, found on PATH or given,
    /// never by the version it points at, past cmux's shims as before.
    #[test]
    fn a_linked_codex_is_kept_as_its_link() {
        let dir = tempfile::tempdir().unwrap();
        let version = dir.path().join("versions/0.155.1");
        stub(&version);
        let shim = dir.path().join("cmux-cli-shims/s1");
        stub(&shim.join("codex"));
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&version, bin.join("codex")).unwrap();
        let search = std::env::join_paths([&shim, &bin]).unwrap();
        for given in [Path::new("codex"), &bin.join("codex")] {
            let found = executable_on(given, &search).unwrap();
            assert_eq!(found, bin.join("codex"), "{}", given.display());
            assert!(!found.starts_with(dir.path().join("versions")));
        }
        // The version given is gone: the name finds the link again, which
        // points to a new version of the same installation.
        let codex = Codex::new(dir.path().join("versions/0.154.0"));
        assert_eq!(
            relocated(&codex.executable, CODEX_NAME, |path| executable_on(
                path, &search
            )),
            Some(bin.join("codex"))
        );
        assert_eq!(
            relocated(&bin.join("codex"), CODEX_NAME, |path| executable_on(
                path, &search
            )),
            None
        );
    }

    #[test]
    fn the_rules_are_written_once_and_kept_out_of_git() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        fs::write(common.join("info/exclude"), "# excludes").unwrap();
        write_rules(&worktree).unwrap();
        write_rules(&worktree).unwrap();
        assert_eq!(
            fs::read_to_string(worktree.join(RULES_PATH)).unwrap(),
            RULES
        );
        assert_eq!(
            fs::read_to_string(common.join("info/exclude")).unwrap(),
            "# excludes\n/.codex/rules/dagq-deny.rules\n"
        );
    }

    #[test]
    fn a_claude_model_is_left_to_codex_and_the_effort_is_given() {
        let codex = Codex::new("codex".into());
        let mut command = CommandSpec::new("codex");
        command.args(["exec", "--json", "--", "prompt"]);
        codex.select_model(&mut command, "claude-opus-5-5", "max");
        codex.select_model(&mut command, "gpt-6-astra", "low");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "--json",
                "-c",
                r#"model_reasoning_effort="xhigh""#,
                "-m",
                "gpt-6-astra",
                "-c",
                r#"model_reasoning_effort="low""#,
                "--",
                "prompt"
            ]
        );
    }

    /// A prompt past the system's limit on the arguments (about 1 MB on
    /// macOS) starts a headless job and a review: the stub `codex` reads it
    /// whole on its standard input, and its arguments do not carry it (task
    /// 1560).
    #[test]
    fn a_codex_job_starts_with_a_prompt_past_the_argument_limit() {
        use crate::infrastructure::process::stub_agent;
        let dir = tempfile::tempdir().unwrap();
        let (worktree, run_dir) = (dir.path().join("worktree"), dir.path().join("run"));
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        let codex = Codex::new(stub_agent::write(dir.path()));
        let prompt = "p".repeat(2 << 20);
        let job = codex
            .headless_command(dir.path(), &prompt, JobAccess::ReadFilesAndQueueCli)
            .unwrap();
        let review = codex
            .review_command(&run(&worktree, &run_dir), &prompt, JobAccess::ReadFiles)
            .unwrap();
        for command in [job, review] {
            let (args, stdin) = stub_agent::run(&command, dir.path());
            assert_eq!(stdin, prompt.len());
            assert!(args < 4096, "{args} bytes of arguments");
        }
    }

    /// A headless job (ADR-t1063-1): `codex exec --json` in the read-only
    /// sandbox in its directory, no bypass flag, no session id, its model
    /// only when Codex's and its effort as `-c`.
    #[test]
    fn a_job_runs_in_the_read_only_sandbox() {
        use crate::domain::actor_model::{ModelRole, RoleModels};
        let codex = Codex::new("/opt/codex".into());
        let cwd = Path::new("/repo");
        let mut command = codex
            .headless_command(cwd, "review it", JobAccess::ReadFilesAndQueueCli)
            .unwrap();
        codex.assign_session_id(&mut command, "not-given");
        let mut models = RoleModels::default();
        models.entry(ModelRole::GoalReview).provider = Some(crate::domain::Provider::Codex);
        codex.apply_launch(&mut command, &models.launch(ModelRole::GoalReview));
        models.entry(ModelRole::GoalReview).model = Some("gpt-6-astra".into());
        models.entry(ModelRole::GoalReview).effort = Some("max".into());
        let mut chosen = codex
            .headless_command(cwd, "review it", JobAccess::ReadFiles)
            .unwrap();
        codex.apply_launch(&mut chosen, &models.launch(ModelRole::GoalReview));
        let args = |command: &CommandSpec| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(command.get_program(), "/opt/codex");
        assert_eq!(command.get_current_dir(), Some(cwd));
        assert_eq!(
            args(&command),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "-C",
                "/repo",
                "-c",
                r#"model_reasoning_effort="medium""#,
            ]
        );
        // The prompt is its standard input, never an argument (task 1560).
        assert_eq!(command.get_stdin(), Some("review it"));
        assert_eq!(
            args(&chosen)[7..],
            [
                "-m",
                "gpt-6-astra",
                "-c",
                r#"model_reasoning_effort="xhigh""#,
            ]
        );
        // Reaching the queue service, the job runs in the profile that
        // allows its socket in place of the read-only sandbox, and nothing
        // else changes (ADR-t1233-5 decision 4).
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("queue.sock");
        let mut reaching = command.clone();
        codex.reach_queue_service(&mut reaching, &socket);
        let real = dir.path().canonicalize().unwrap().join("queue.sock");
        let profile = job_service_config(&socket).unwrap();
        assert_eq!(
            profile.last().unwrap(),
            &format!(
                r#"permissions.dagq_job.network.unix_sockets={{{}="allow"}}"#,
                serde_json::to_string(real.to_str().unwrap()).unwrap()
            )
        );
        let mut expected = vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "-C".to_owned(),
            "/repo".to_owned(),
            "-c".to_owned(),
            r#"model_reasoning_effort="medium""#.to_owned(),
        ];
        for config in &profile {
            expected.extend(["-c".to_owned(), config.clone()]);
        }
        assert_eq!(args(&reaching), expected);
        assert_eq!(reaching.get_stdin(), Some("review it"));
        assert!(profile.contains(&r#"permissions.dagq_job.extends=":read-only""#.to_owned()));
        assert!(profile.contains(&"features.network_proxy=true".to_owned()));
        // A command without the read-only sandbox (a worker's turn) is left
        // as it is.
        let mut twice = reaching.clone();
        codex.reach_queue_service(&mut twice, &socket);
        assert_eq!(twice, reaching);
        for args in [args(&command), args(&chosen), args(&reaching)] {
            assert!(
                !args.iter().any(|arg| arg.contains("dangerously")
                    || arg.contains("bypass")
                    || arg.contains("writable_roots")
                    || arg.contains("trust_level")),
                "{args:?}"
            );
        }
    }

    /// The reply, thread, model and failure of a job are read from its
    /// JSONL and the thread's rollout (spike 3. and 4.).
    #[test]
    fn a_job_reads_its_reply_thread_model_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let verdict = r#"{"verdict":"achieved","criteria":[],"summary":"done"}"#;
        let line = |value: serde_json::Value| format!("{value}\n");
        let stdout = [
            line(serde_json::json!({"type": "thread.started", "thread_id": "t-1"})),
            line(serde_json::json!({"type": "turn.started"})),
            line(serde_json::json!({"type": "item.completed", "item": {"type": "agent_message", "text": "Looking."}})),
            line(serde_json::json!({"type": "item.completed", "item": {"type": "command_execution", "command": "dagq show 1", "exit_code": 0}})),
            line(serde_json::json!({"type": "item.completed", "item": {"type": "agent_message", "text": verdict}})),
            line(serde_json::json!({"type": "turn.completed", "usage": {"input_tokens": 3, "output_tokens": 1}})),
        ]
        .concat();
        let codex = Codex {
            executable: "codex".into(),
            home: Some(dir.path().to_owned()),
        };
        assert_eq!(codex.job_reply(&stdout), verdict);
        assert_eq!(codex.job_reply("not json\n"), "");
        let unknown = codex.job_session(&stdout, None).unwrap();
        assert_eq!(unknown.session_id.as_deref(), Some("t-1"));
        assert_eq!(unknown.model, None);
        assert!(
            unknown.model_unknown.unwrap().contains("no rollout"),
            "the rollout is not written yet"
        );
        let day = dir.path().join("sessions/2026/09/29");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            day.join("rollout-2026-09-29T00-00-00-t-1.jsonl"),
            line(serde_json::json!({"timestamp": "2026-09-29T00:00:01.000Z", "type": "turn_context", "payload": {"model": "gpt-6-astra"}})),
        )
        .unwrap();
        let session = codex.job_session(&stdout, None).unwrap();
        assert_eq!(session.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(session.model_unknown, None);
        // A turn_context from before the job is an earlier thread's turn.
        let later = crate::domain::stats::rfc3339_millis("2026-09-29T01:00:00.000Z");
        assert_eq!(codex.job_session(&stdout, later).unwrap().model, None);
        assert_eq!(codex.job_failure(&stdout, ""), JobFailure::Other);
        let failed = |message: &str| {
            line(serde_json::json!({"type": "thread.started", "thread_id": "t-2"}))
                + &line(serde_json::json!({"type": "error", "message": message}))
                + &line(serde_json::json!({"type": "turn.failed", "error": {"message": message}}))
        };
        assert_eq!(
            codex.job_failure(
                &failed("unexpected status 401 Unauthorized: Missing bearer"),
                ""
            ),
            JobFailure::Authentication
        );
        assert_eq!(
            codex.job_failure(&failed("You've hit your usage limit."), ""),
            JobFailure::UsageLimit
        );
        assert_eq!(
            codex.job_failure(&failed("the model broke"), ""),
            JobFailure::Other
        );
        assert_eq!(
            codex.job_failure("", "error: 401 Unauthorized\n"),
            JobFailure::Authentication
        );
    }

    fn run(worktree: &Path, run_dir: &Path) -> TaskRun {
        use crate::domain::{CommitSha, Provider, RunId, RunStatus, TaskId};
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap(),
            task_id: TaskId::new(15),
            status: RunStatus::Running,
            requested_provider: Provider::Codex,
            actual_provider: Provider::Codex,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: None,
            worktree_path: Some(worktree.display().to_string()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(run_dir.display().to_string()),
            last_error: None,
            workspace_closed_at: None,
            created_at: "2026-09-28 00:00:00".into(),
        })
        .unwrap()
    }

    #[test]
    fn the_starters_tmpdir_is_a_writable_root_when_absolute() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            starter_tmpdir(Some(dir.path().into())),
            Some(dir.path().canonicalize().unwrap())
        );
        // One that is not there yet is kept as given.
        assert_eq!(
            starter_tmpdir(Some("/nonexistent/t".into())),
            Some(PathBuf::from("/nonexistent/t"))
        );
        assert_eq!(starter_tmpdir(None), None);
        assert_eq!(starter_tmpdir(Some("".into())), None);
        assert_eq!(starter_tmpdir(Some("rel/t".into())), None);
    }

    #[test]
    fn a_turn_starts_or_resumes_the_thread_with_the_same_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let (common, worktree) = layout(dir.path());
        let run_dir = dir.path().join("run");
        let run = run(&worktree, &run_dir);
        let codex = Codex::new("/bin/codex".into());
        let args = |command: &CommandSpec| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        let starter = starter_tmpdir(std::env::var_os("TMPDIR"));
        let mut expected_roots =
            writable_roots(&worktree, &run_dir, &cargo_home().unwrap()).unwrap();
        expected_roots.extend(starter.clone());
        let roots = writable_roots_config(&expected_roots).unwrap();
        // The starter's `$TMPDIR` stays writable though the turn's
        // `TMPDIR` is the run directory's (task 1290).
        if let Some(starter) = &starter {
            assert!(
                roots.ends_with(&format!(",\"{}\"]", starter.display())),
                "{roots}"
            );
        }
        let trust = trust_config(&worktree).unwrap();
        assert_eq!(
            trust,
            format!(
                r#"projects={{"{}"={{trust_level="trusted"}}}}"#,
                worktree.display()
            )
        );
        let first = codex
            .turn_command(
                &TurnTarget::of_run(&run).unwrap(),
                "-do it",
                TurnSession::New("ignored"),
            )
            .unwrap();
        assert_eq!(first.get_program(), "/bin/codex");
        assert_eq!(first.get_current_dir(), Some(worktree.as_path()));
        assert!(first.get_new_session());
        let sandbox = [
            "-c",
            SANDBOX_CONFIG[0],
            "-c",
            SANDBOX_CONFIG[1],
            "-c",
            SANDBOX_CONFIG[2],
            "-c",
            roots.as_str(),
            "-c",
            trust.as_str(),
        ];
        let worktree_text = worktree.display().to_string();
        let mut expected = vec!["exec", "--json", "-C", worktree_text.as_str()];
        expected.extend(sandbox);
        expected.extend(["--", "-do it"]);
        assert_eq!(args(&first), expected);
        let resumed = codex
            .turn_command(
                &TurnTarget::of_run(&run).unwrap(),
                "answer",
                TurnSession::Resume("th-1"),
            )
            .unwrap();
        assert_eq!(resumed.get_current_dir(), Some(worktree.as_path()));
        let mut expected = vec!["exec", "resume", "--json"];
        expected.extend(sandbox);
        expected.extend(["--", "th-1", "answer"]);
        assert_eq!(args(&resumed), expected);
        // A `required` run's turn is refused: Codex gets no broker tools
        // yet, and never runs with its own instead (ADR-t838-1).
        let config = run_dir.join("broker/mcp.json");
        let error = codex
            .turn_command(
                &TurnTarget {
                    broker_required: Some(&config),
                    ..TurnTarget::of_run(&run).unwrap()
                },
                "-do it",
                TurnSession::New("ignored"),
            )
            .unwrap_err();
        assert!(
            crate::application::broker_run::BrokerRequiredRefused::is(&error),
            "{error:#}"
        );
        // Both keep their temporary files in the run directory's `tmp`,
        // made for them inside a writable root (task 1290).
        let tmp = run_dir.join(RUN_TMP_DIR);
        assert!(tmp.is_dir());
        assert!(roots.contains(&format!("\"{}\"", run_dir.display())));
        for command in [&first, &resumed] {
            let tmpdirs: Vec<_> = command
                .get_envs()
                .filter(|(key, _)| *key == "TMPDIR")
                .collect();
            assert_eq!(tmpdirs, [("TMPDIR".as_ref(), Some(tmp.as_os_str()))]);
        }
        // The sandbox is workspace-write as before, which lets `/tmp` be
        // written too; the starter's `$TMPDIR` is a writable root above
        // (ADR-t813-3 decision 2).
        assert_eq!(SANDBOX_CONFIG[0], r#"sandbox_mode="workspace-write""#);
        // A link the worker put in its place is not followed: the next turn
        // gets a real directory again, and what the link named is kept.
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        fs::remove_dir_all(&tmp).unwrap();
        std::os::unix::fs::symlink(&outside, &tmp).unwrap();
        codex
            .turn_command(
                &TurnTarget::of_run(&run).unwrap(),
                "again",
                TurnSession::Resume("th-1"),
            )
            .unwrap();
        assert!(!tmp.is_symlink() && tmp.is_dir());
        assert!(outside.join("keep").is_file());
        // So is a file.
        fs::remove_dir(&tmp).unwrap();
        fs::write(&tmp, "x").unwrap();
        assert_eq!(run_tmp_dir(&run_dir).unwrap(), tmp);
        assert!(tmp.is_dir());
        // The worker's `dagq ask` goes to the queue service (ADR-t1233-5
        // decision 5), which the open network of its sandbox reaches as it
        // is.
        for command in [&first, &resumed] {
            let mut reaching = command.clone();
            codex.reach_queue_service(&mut reaching, Path::new("/q/service/queue.sock"));
            assert_eq!(reaching, *command);
        }
        // The rules are in the worktree, excluded.
        assert!(worktree.join(RULES_PATH).is_file());
        assert!(
            fs::read_to_string(common.join("info/exclude"))
                .unwrap()
                .contains(RULES_PATH)
        );
        assert!(codex.command(&run, "p").is_err());
        assert!(codex.resume_command(&run).is_err());
        let review = codex
            .review_command(&run, "p", JobAccess::ReadFiles)
            .unwrap();
        assert_eq!(review.get_current_dir(), Some(worktree.as_path()));
        // The review is a job in the worktree, which it is told to
        // distrust so that the worker's `.codex` does not reach it
        // (ADR-t1570-1); the worker's turns trust it as before.
        let distrust = distrust_config(&worktree).unwrap();
        assert_eq!(
            distrust,
            format!(
                r#"projects={{"{}"={{trust_level="untrusted"}}}}"#,
                worktree.display()
            )
        );
        assert_eq!(
            args(&review),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "-C",
                worktree_text.as_str(),
                "-c",
                distrust.as_str(),
            ]
        );
        assert_eq!(review.get_stdin(), Some("p"));
        let job = codex
            .headless_command(&worktree, "p", JobAccess::ReadFiles)
            .unwrap();
        assert_eq!(args(&review)[..7], args(&job)[..]);
        // Its model and effort, and the profile that reaches the queue
        // service in place of the read-only sandbox, are added as to any
        // job, and the distrust stays.
        use crate::domain::actor_model::{ModelRole, RoleModels};
        let mut models = RoleModels::default();
        models.entry(ModelRole::Review).provider = Some(crate::domain::Provider::Codex);
        models.entry(ModelRole::Review).model = Some("gpt-6-astra".into());
        models.entry(ModelRole::Review).effort = Some("low".into());
        let mut launched = review.clone();
        codex.apply_launch(&mut launched, &models.launch(ModelRole::Review));
        let socket = dir.path().join("queue.sock");
        codex.reach_queue_service(&mut launched, &socket);
        let mut expected = vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "-C".to_owned(),
            worktree_text.clone(),
            "-c".to_owned(),
            distrust.clone(),
            "-m".to_owned(),
            "gpt-6-astra".to_owned(),
            "-c".to_owned(),
            r#"model_reasoning_effort="low""#.to_owned(),
        ];
        for config in job_service_config(&socket).unwrap() {
            expected.extend(["-c".to_owned(), config]);
        }
        assert_eq!(args(&launched), expected);
        assert_eq!(launched.get_stdin(), Some("p"));
        // The worker's turns are given no distrust.
        for command in [&first, &resumed] {
            assert!(!args(command).contains(&distrust), "{:?}", args(command));
        }
        // Codex runs no review subagents (ADR-t1453-1 decision 8): it says
        // so, refuses them, and its review command is as before.
        assert!(!codex.runs_review_subagents());
        let mut handed = review.clone();
        let agent = crate::domain::review_subagents::AgentDefinition::read("design", "Check it.\n");
        assert!(codex.review_subagents(&mut handed, &[agent]).is_err());
        assert_eq!(handed, review);
        assert!(codex.turn_session_from_output());
        assert!(codex.turn_reader().is_ok());
        assert!(Codex::new("/nonexistent/codex".into()).preflight().is_err());
        // Its rollouts are under its home's `sessions`.
        let at = Codex {
            home: Some("/h".into()),
            ..codex
        };
        assert_eq!(at.sessions_dir(), Some(PathBuf::from("/h/sessions")));
    }

    /// An agent's job's tools (ADR-t1728-2): a review's default (the
    /// reads) and any declaration with a read or the shell keep Codex's
    /// shell, which its reads go through, and one without them turns it
    /// off; either way the read-only sandbox, the profile that reaches the
    /// queue service in its place, and the run's review's distrust
    /// (ADR-t1570-1) stay as they were.
    #[test]
    fn an_agent_jobs_declared_tools_only_narrow_codexs_job() {
        use crate::domain::review_subagents::AgentRole;
        let dir = tempfile::tempdir().unwrap();
        let codex = Codex::new("/bin/codex".into());
        let args = |command: &CommandSpec| -> Vec<String> {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            agent_job_tools_config(&AgentRole::Review.default_tools()),
            Vec::<String>::new()
        );
        assert_eq!(
            agent_job_tools_config(&AgentTools::new([AgentTool::Glob])),
            Vec::<String>::new()
        );
        assert_eq!(
            agent_job_tools_config(&AgentTools::new([AgentTool::Shell])),
            Vec::<String>::new()
        );
        let none = agent_job_tools_config(&AgentTools::new([]));
        assert_eq!(
            none,
            ["features.shell_tool=false", "features.unified_exec=false"]
        );
        assert_eq!(
            agent_job_tools_config(&AgentTools::new([AgentTool::Edit, AgentTool::Write])),
            none
        );
        // The run's review's launch, given the configs: only `-c`s are
        // added after it, and the sandbox and the distrust stay.
        let distrust = distrust_config(dir.path()).unwrap();
        let mut review = codex
            .headless_command(dir.path(), "p", JobAccess::ReadFiles)
            .unwrap();
        review.arg("-c").arg(&distrust);
        let before = args(&review);
        let mut narrowed = review.clone();
        for config in &none {
            narrowed.option_args(["-c".to_owned(), config.clone()]);
        }
        let after = args(&narrowed);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(
            after[before.len()..],
            [
                "-c",
                "features.shell_tool=false",
                "-c",
                "features.unified_exec=false"
            ]
        );
        assert!(
            after
                .windows(2)
                .any(|pair| pair == ["--sandbox", JOB_SANDBOX])
        );
        // Reaching the queue service swaps the sandbox for the job's
        // profile as before.
        let socket = dir.path().join("queue.sock");
        codex.reach_queue_service(&mut narrowed, &socket);
        let reaching = args(&narrowed);
        assert!(!reaching.contains(&"--sandbox".to_owned()));
        for config in job_service_config(&socket)
            .unwrap()
            .into_iter()
            .chain([distrust.clone()])
            .chain(none.clone())
        {
            assert!(reaching.contains(&config), "{config} in {reaching:?}");
        }
    }
}
