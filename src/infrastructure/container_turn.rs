//! `dagq relay turn`: what a worker's turn runs inside its container
//! ([Container runtime], ADR-t2179-1 decisions 2 and 6, ADR-t2114-2
//! decision 1, ADR-t2114-3 decision 3). It is the only place that runs git
//! in a container run's clone, beside the worker itself: the control side
//! runs none there.
//!
//! In order: the run's state the last turn left is marked as of a turn
//! that has not ended ([`mark_turn_started`]); the restore of a parked
//! run's work the control side handed is applied to the clone and answered
//! ([`restore_clone`]), a clone that cannot be restored running no agent;
//! main's bundle the control side left in the run directory is fetched
//! into the clone's [`CLONE_MAIN_REF`]; the gate of the container's
//! sccache server is waited for when the supervisor starts one
//! ([`sccache_env`]); the forwarder starts on the container's loopback;
//! the agent runs in the clone with [`turn_env`], made from the run
//! token's file and the run; and whatever the agent did, the run branch's
//! bundle, the clone's uncommitted work and the run's
//! state are written to the run directory, each to a temporary file
//! renamed into place. Its exit code is the agent's. It writes nothing to
//! its standard output, which is the agent's (the turn's output the
//! wrapper reads).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fs;
use std::io::{ErrorKind, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::domain::actor::RUN_ID_ENV;
use crate::domain::broker_relay::socket_path;
use crate::domain::clone_work::{
    self, CloneWork, ExecutorRestore, Part, PartKind, RESTORE_RESULT_FILE, RUN_RESTORE_FILE,
    RUN_WORK_FILE, RestoreRequest, RestoreResult,
};
use crate::domain::queue_service::CREDENTIAL_FILE_ENV;
use crate::domain::replan_snapshot::{RestoreCheck, check_relative, compare};
use crate::domain::run_clone::{
    self, CLONE_MAIN_REF, MAIN_BUNDLE_FILE, RUN_BUNDLE_FILE, RUN_STATE_FILE, RunState, StateFound,
    TurnStart,
};
use crate::domain::run_container::{TOKEN_FILE, turn_env, verification_env};
use crate::domain::sccache::{
    CONTAINER_GATE_VAR, CONTAINER_TURN_VAR, ContainerGate, GUARD_PROGRAM_VAR, GateStep,
    WRAPPER_VAR, container_agent_env, container_guard, container_request, gate_step,
};

/// How `dagq relay turn` runs.
#[derive(Debug, Clone)]
pub struct TurnOptions {
    pub run_dir: PathBuf,
    pub clone: PathBuf,
    /// The run's base commit: the prerequisite of the run branch's bundle.
    pub base: String,
    /// The run branch (`dagq/<run id>`).
    pub branch: String,
    /// The agent's command line.
    pub agent: Vec<String>,
    /// The forwarder's port: [`FORWARD_PORT`].
    pub port: u16,
}

/// Run the turn of [`TurnOptions`] with the variables `env` reads: the
/// agent's exit code (128 and the signal's number for an agent killed by
/// one). An error before the agent starts (no run, no token, a state that
/// cannot be marked) runs no agent and writes no state of the turn's end.
pub fn run_turn(options: &TurnOptions, env: impl Fn(&str) -> Option<String>) -> Result<i32> {
    let run = env(RUN_ID_ENV).with_context(|| format!("{RUN_ID_ENV} names no run"))?;
    let token_file =
        env(CREDENTIAL_FILE_ENV).map_or_else(|| options.run_dir.join(TOKEN_FILE), PathBuf::from);
    let token = fs::read_to_string(&token_file)
        .with_context(|| format!("read the run token's file {}", token_file.display()))?;
    let token = token.trim();
    ensure!(!token.is_empty(), "{} holds no token", token_file.display());
    // Before the agent can change the clone: a turn cut off with its
    // container leaves the state marked, not taken for its work.
    mark_turn_started(&options.run_dir)?;
    // A parked run's work comes back before the agent sees the clone; one
    // that cannot runs no agent, and the control side reads why.
    restore_clone(&options.clone, &options.run_dir, &options.branch)?;
    // A bundle that cannot be fetched leaves the ref where it was: the
    // agent's rebase then finds main as it was, and the control side asks
    // again; the turn itself goes on.
    if let Err(error) = import_main(&options.clone, &options.run_dir) {
        eprintln!("dagq: main's bundle was not fetched into {CLONE_MAIN_REF}: {error:#}");
    }
    // Before the agent: the server the supervisor starts in this
    // container is up, or known not to be.
    let sccache = sccache_env(&options.run_dir, &env, GATE_WAIT)?;
    let stop = Arc::new(AtomicBool::new(false));
    let forwarder = start_forwarder(options.port, stop.clone());
    let (program, args) = options
        .agent
        .split_first()
        .context("no agent to run in the turn")?;
    let status = Command::new(program)
        .args(args)
        .current_dir(&options.clone)
        .envs(turn_env(&run, token))
        // A turn whose sccache the supervisor handles was started without
        // the image's `RUSTC_WRAPPER`, so the agent's is only the guard
        // these give behind a started gate.
        .envs(sccache)
        // The turn's prompt, which podman's `--interactive` passes on.
        .stdin(Stdio::inherit())
        .status()
        .with_context(|| format!("run {program}"));
    stop.store(true, Ordering::SeqCst);
    if let Some(forwarder) = forwarder {
        let _ = forwarder.join();
    }
    // The run's facts are written whatever the agent did, so the control
    // side reads this turn's end and never an earlier one's.
    if let Err(error) = write_turn_end(
        &options.clone,
        &options.run_dir,
        &options.base,
        &options.branch,
    ) {
        eprintln!("dagq: the run's bundle, work and state were not written: {error:#}");
    }
    let status = status?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

/// How long the launcher waits for the gate of the container's sccache
/// server before it writes its own and runs the agent without
/// `RUSTC_WRAPPER`: longer than the supervisor takes to see the request
/// (a pass) and start the server and make the guard in the container
/// (its `podman exec`s), so a gate is missed only when no supervisor looks.
pub const GATE_WAIT: Duration = Duration::from_secs(180);

/// The agent's variables about sccache (ADR-t2114-4 decision 2), when the
/// container's environment (read with `env`) names the gate of a server
/// the supervisor starts ([`CONTAINER_GATE_VAR`]); none otherwise, the
/// container's `[run.env]` left as it is. It leaves the request named by
/// the turn's token ([`CONTAINER_TURN_VAR`], [`container_request`]) in the
/// gate's directory, waits up to `wait` for
/// the gate, writes its own where none came ([`gate_step`]), and gives the
/// agent what the gate there says ([`container_agent_env`]): behind a
/// started gate, the guard the supervisor made in the agent's home in
/// `run_dir`. `Err` when no gate could be settled, which runs no agent.
pub fn sccache_env(
    run_dir: &Path,
    env: impl Fn(&str) -> Option<String>,
    wait: Duration,
) -> Result<Vec<(String, String)>> {
    let Some(gate_file) = env(CONTAINER_GATE_VAR) else {
        return Ok(Vec::new());
    };
    let gate_file = PathBuf::from(gate_file);
    let dir = gate_file
        .parent()
        .context("the sccache gate has no directory")?;
    let program = env(GUARD_PROGRAM_VAR).unwrap_or_default();
    // A request that cannot be left is waited out as any unanswered one.
    let left = env(CONTAINER_TURN_VAR)
        .as_deref()
        .and_then(container_request)
        .context("the turn has no token to name its request by")
        .and_then(|name| write_file(dir, &name, b""));
    if let Err(error) = left {
        eprintln!("dagq: the request for the container's sccache server was not left: {error:#}");
    }
    let gate = wait_for_gate(&gate_file, wait)?;
    if let ContainerGate::Failed(why) | ContainerGate::TimedOut(why) = &gate {
        eprintln!("dagq: {why}; the agent compiles without {WRAPPER_VAR}");
    }
    let guard = container_guard(run_dir);
    let guard = guard.to_str().context("the guard's path is not UTF-8")?;
    Ok(container_agent_env(&program, &gate, guard))
}

/// The gate in `file` the launcher obeys: the one there within `wait`,
/// else its own [`ContainerGate::TimedOut`] written where none is, else
/// the one written there first. Its own names itself (a token no other
/// gate has), so the supervisor records it once whenever it finds it.
fn wait_for_gate(file: &Path, wait: Duration) -> Result<ContainerGate> {
    let started = std::time::Instant::now();
    loop {
        // One there that cannot be read says nothing yet: it is waited for
        // as an empty one.
        let read = match fs::read_to_string(file) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(_) => Some(String::new()),
        };
        match gate_step(read.as_deref(), started.elapsed(), wait) {
            GateStep::Wait => {}
            GateStep::Obey(gate) => return Ok(gate),
            GateStep::Claim => {
                let gate = ContainerGate::TimedOut(format!(
                    "the container's launcher found no sccache gate within {wait:?} (gate {})",
                    uuid::Uuid::new_v4().simple()
                ));
                match write_new(file, gate.text().as_bytes()) {
                    Ok(()) => return Ok(gate),
                    // The supervisor's came meanwhile: that one is obeyed.
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        bail!("the sccache gate could not be written: {error}; no agent runs")
                    }
                }
            }
            GateStep::Abort(why) => bail!("{why}; no agent runs"),
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Write `bytes` as `path` only where nothing is there, whole: to a file
/// of its own first, then linked to `path`, which fails where an entry is.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map_or_else(Default::default, |name| name.to_string_lossy()),
        uuid::Uuid::new_v4().simple()
    ));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::hard_link(&tmp, path));
    let _ = fs::remove_file(&tmp);
    written
}

/// A facts-only turn (`dagq relay turn --facts-only`): what the control
/// side starts when the facts a run's last turn left are not of its end
/// (that turn was cut off with its container), before it carries the run's
/// work over, in a container with no path to the queue broker and no run
/// token. No agent runs and no forwarder starts: the state is marked as at
/// any turn's start, the restore the control side handed is applied as
/// before an agent ([`restore_clone`]), and the run branch's bundle, the
/// clone's work and the run's state are written as at a turn's end
/// ([`write_turn_end`]), every git of them guarded ([`CloneGit::guarded`])
/// so that nothing the worker left in the clone runs after its run
/// stopped. `Err` when any of them fails, which leaves no state of a
/// turn's end.
pub fn run_facts_turn(clone: &Path, run_dir: &Path, base: &str, branch: &str) -> Result<()> {
    let clone = CloneGit::guarded(clone)?;
    mark_turn_started(run_dir)?;
    restore_in(&clone, run_dir, branch)?;
    turn_end_in(&clone, run_dir, base, branch).map(drop)
}

/// How `dagq relay verify` runs.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    /// The copy of the verification's token.
    pub token_file: PathBuf,
    /// The verification command's line.
    pub command: Vec<String>,
    /// The forwarder's port: [`FORWARD_PORT`].
    pub port: u16,
}

/// Run one verification command of `integrate` in its container
/// ([Container runtime]): start the forwarder, give the command the proxy
/// variables of [`verification_env`] from the token's file and the run
/// [`RUN_ID_ENV`] names (read with `env`), and run it in the current
/// directory (the verification's clone). Its output is the command's, and
/// so is its exit code (128 and the signal's number for a command killed
/// by one). An error before the command starts (no run, no token) runs no
/// command.
///
/// [Container runtime]: ../../docs/design/container-runtime.md
pub fn run_verification(
    options: &VerifyOptions,
    env: impl Fn(&str) -> Option<String>,
) -> Result<i32> {
    let run = env(RUN_ID_ENV).with_context(|| format!("{RUN_ID_ENV} names no run"))?;
    let token = fs::read_to_string(&options.token_file).with_context(|| {
        format!(
            "read the verification token's file {}",
            options.token_file.display()
        )
    })?;
    let token = token.trim();
    ensure!(
        !token.is_empty(),
        "{} holds no token",
        options.token_file.display()
    );
    let (program, args) = options
        .command
        .split_first()
        .context("no verification command to run")?;
    let stop = Arc::new(AtomicBool::new(false));
    let forwarder = start_forwarder(options.port, stop.clone());
    let status = Command::new(program)
        .args(args)
        .envs(verification_env(&run, token))
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("run {program}"));
    stop.store(true, Ordering::SeqCst);
    if let Some(forwarder) = forwarder {
        let _ = forwarder.join();
    }
    let status = status?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

/// The forwarder on the container's loopback, on a thread until `stop`;
/// `None` when the port cannot be taken (said on stderr): the agent then
/// reaches nothing outside, which the broker's refusals would not tell.
fn start_forwarder(port: u16, stop: Arc<AtomicBool>) -> Option<thread::JoinHandle<u64>> {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("dagq: the forwarder cannot listen on 127.0.0.1:{port}: {error}");
            return None;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("dagq: the forwarder cannot listen on 127.0.0.1:{port}: {error}");
        return None;
    }
    let socket = PathBuf::from(socket_path());
    Some(thread::spawn(move || {
        super::broker_relay::forward_on(&listener, &socket, &stop, Duration::from_millis(50))
    }))
}

/// The clone the executor's git runs in, and the configuration it runs
/// with: the worker's own in a worker's turn ([`Self::plain`]), or with
/// the guard of a facts-only turn ([`Self::guarded`]). It derefs to the
/// clone's path.
struct CloneGit<'a> {
    path: &'a Path,
    /// The environment that carries the guard's overrides; none when plain.
    guard: Vec<(String, String)>,
}

impl<'a> CloneGit<'a> {
    fn plain(path: &'a Path) -> Self {
        Self {
            path,
            guard: Vec::new(),
        }
    }

    /// Git in `path` that runs no program the worker could have left in the
    /// clone's configuration or tracked content: no hook, no fsmonitor and
    /// no driver some scope defines ([`facts_overrides`]), passed as the
    /// command scope, which wins over the clone's. `Err` when the
    /// configuration cannot be read or the guard does not hold, and no
    /// git is to run then.
    fn guarded(path: &'a Path) -> Result<Self> {
        let plain = Self::plain(path);
        let names = git_ok(&plain, &["config", "--list", "--name-only", "-z"])?;
        let overrides = facts_overrides(&names).map_err(anyhow::Error::msg)?;
        let guarded = Self {
            path,
            guard: run_clone::guard_env(&overrides),
        };
        let effective = git_ok(&guarded, &["config", "--list", "-z"])?;
        run_clone::guard_holds(&effective, &overrides).map_err(anyhow::Error::msg)?;
        Ok(guarded)
    }

    /// For a guarded git, refuse a clone with a populated submodule (a
    /// gitlink of its index whose directory holds a `.git`): `status` and
    /// `diff` run git in it, which reads the nested repository's own
    /// configuration, whose drivers the guard does not override. Nothing
    /// for a plain git.
    fn refuse_nested(&self) -> Result<()> {
        if self.guard.is_empty() {
            return Ok(());
        }
        let staged = git_ok(self, &["ls-files", "--stage", "-z"])?;
        for entry in staged.split('\0').filter(|entry| !entry.is_empty()) {
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            if meta.starts_with("160000 ")
                && fs::symlink_metadata(self.join(path).join(".git")).is_ok()
            {
                bail!(
                    "the clone has the populated submodule {path}, whose own configuration the facts-only turn's guard does not reach"
                );
            }
        }
        Ok(())
    }
}

impl std::ops::Deref for CloneGit<'_> {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path
    }
}

/// The overrides of a facts-only turn's git for the configuration whose
/// names are `names` (`git config --list --name-only -z`): the control
/// side's guard ([`run_clone::guard_overrides`], no hook and no driver)
/// and no fsmonitor, whose program `git status` would run.
fn facts_overrides(names: &str) -> std::result::Result<Vec<(String, String)>, String> {
    let mut overrides = run_clone::guard_overrides(names)?;
    overrides.push(("core.fsmonitor".to_owned(), "false".to_owned()));
    Ok(overrides)
}

/// Git in the clone, with the configuration git reads there and the
/// clone's guard, if any.
fn git(clone: &CloneGit, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(clone.path)
        .args(args)
        .envs(clone.guard.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))
}

fn git_ok(clone: &CloneGit, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_bytes(clone, args)?).into_owned())
}

/// The output of git in the clone, as bytes; `Err` when it failed.
fn git_bytes(clone: &CloneGit, args: &[&str]) -> Result<Vec<u8>> {
    let output = git(clone, args)?;
    if !output.status.success() {
        bail!(
            "git {} exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Git in the clone with `input` on its standard input.
fn git_with_input(clone: &CloneGit, args: &[&str], input: &[u8]) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(clone.path)
        .args(args)
        .envs(clone.guard.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    let written = child
        .stdin
        .take()
        .context("git's standard input")?
        .write_all(input);
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "git {} exited with {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    written.with_context(|| format!("write to git {}", args.join(" ")))
}

/// Fetch main's bundle of `run_dir` into the clone's [`CLONE_MAIN_REF`]
/// (ADR-t2179-1 decision 6): its one ref, forced, without tags and
/// without a `FETCH_HEAD`. Nothing while there is no bundle (main is still
/// the base) or the path holds anything but a plain file. `true` when it
/// was fetched.
pub fn import_main(clone: &Path, run_dir: &Path) -> Result<bool> {
    let clone = &CloneGit::plain(clone);
    let bundle = run_dir.join(MAIN_BUNDLE_FILE);
    match fs::symlink_metadata(&bundle) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => bail!("{} is not a plain file", bundle.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("look at {}", bundle.display())),
    }
    let bundle = bundle.to_string_lossy().into_owned();
    let heads = git_ok(clone, &["bundle", "list-heads", &bundle])?;
    let heads: Vec<&str> = heads.lines().filter(|line| !line.is_empty()).collect();
    let [head] = heads.as_slice() else {
        bail!("main's bundle holds {} refs, not one", heads.len());
    };
    let reference = head
        .split_once(' ')
        .map(|(_, name)| name)
        .filter(|name| name.starts_with("refs/heads/"))
        .with_context(|| format!("main's bundle lists no branch: {head:?}"))?;
    git_ok(
        clone,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-recurse-submodules",
            &bundle,
            &format!("+{reference}:{CLONE_MAIN_REF}"),
        ],
    )?;
    Ok(true)
}

/// Mark the run's state in `run_dir` as of a turn that has not ended, or
/// remove what cannot be read as one, as [`run_clone::turn_start`] decides
/// for what is there: no facts an earlier turn left stand as the last
/// turn's while the agent runs.
pub fn mark_turn_started(run_dir: &Path) -> Result<()> {
    let path = run_dir.join(RUN_STATE_FILE);
    let found = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::read(&path)
            .ok()
            .and_then(|bytes| RunState::parse(&String::from_utf8_lossy(&bytes)).ok())
            .map_or(StateFound::Unreadable, StateFound::State),
        Ok(metadata) if metadata.file_type().is_symlink() => StateFound::Unreadable,
        Ok(_) => StateFound::Other,
        Err(error) if error.kind() == ErrorKind::NotFound => StateFound::Nothing,
        Err(error) => return Err(error).with_context(|| format!("look at {}", path.display())),
    };
    match run_clone::turn_start(found) {
        TurnStart::Leave => Ok(()),
        TurnStart::Mark(state) => write_state(run_dir, &state),
        TurnStart::Remove => remove_if_there(&path),
    }
}

/// Write the run's facts of the turn's end in `run_dir` (ADR-t2179-1
/// decision 2): the run branch's bundle on top of `base`, removed while
/// the branch has no new commit, then the clone's uncommitted work
/// ([`CloneWork`], which a replanning's snapshot saves), then the run's
/// state, of the turn's end, both of the generation after the state found
/// ([`run_clone::next_generation`]). Each is written to a temporary file
/// and renamed, in that order; work that cannot be read leaves none, so
/// no earlier turn's stands as this one's.
pub fn write_turn_end(clone: &Path, run_dir: &Path, base: &str, branch: &str) -> Result<RunState> {
    turn_end_in(&CloneGit::plain(clone), run_dir, base, branch)
}

/// [`write_turn_end`] with the executor's git of `clone`.
fn turn_end_in(clone: &CloneGit, run_dir: &Path, base: &str, branch: &str) -> Result<RunState> {
    clone.refuse_nested()?;
    let head = git_ok(clone, &["rev-parse", "--verify", "HEAD^{commit}"])?
        .trim()
        .to_owned();
    let symbolic = git(clone, &["symbolic-ref", "-q", "HEAD"])?;
    let pointed = symbolic
        .status
        .success()
        .then(|| String::from_utf8_lossy(&symbolic.stdout).trim().to_owned());
    let status = git_ok(clone, &["status", "--porcelain", "--untracked-files=all"])?;
    let mut rebase_in_progress = false;
    for dir in ["rebase-merge", "rebase-apply"] {
        let path = git_ok(clone, &["rev-parse", "--git-path", dir])?;
        let path = Path::new(path.trim());
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            clone.join(path)
        };
        rebase_in_progress |= path.exists();
    }
    let branch_ref = format!("refs/heads/{branch}");
    let tip = git(
        clone,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{branch_ref}^{{commit}}"),
        ],
    )?;
    let tip = tip
        .status
        .success()
        .then(|| String::from_utf8_lossy(&tip.stdout).trim().to_owned());
    let bundle = run_dir.join(RUN_BUNDLE_FILE);
    match tip.filter(|tip| tip != base) {
        Some(_) => {
            let tmp = run_dir.join(format!("{RUN_BUNDLE_FILE}.tmp"));
            remove_if_there(&tmp)?;
            git_ok(
                clone,
                &[
                    "bundle",
                    "create",
                    "--quiet",
                    &tmp.to_string_lossy(),
                    &branch_ref,
                    &format!("^{base}"),
                ],
            )?;
            fs::rename(&tmp, &bundle)
                .with_context(|| format!("rename {} into place", tmp.display()))?;
        }
        None => remove_if_there(&bundle)?,
    }
    let generation = run_clone::next_generation(read_state(run_dir).as_ref());
    match read_work(clone, &head) {
        Ok((mut found, contents)) => {
            found.generation = generation;
            write_file(run_dir, RUN_WORK_FILE, &clone_work::pack(&found, &contents))?;
        }
        Err(error) => {
            remove_if_there(&run_dir.join(RUN_WORK_FILE))?;
            eprintln!("dagq: the clone's uncommitted work was not written: {error:#}");
        }
    }
    let state = RunState {
        branch: pointed,
        head,
        status,
        rebase_in_progress,
        turn_ended: Some(true),
        generation: Some(generation),
    };
    write_state(run_dir, &state)?;
    Ok(state)
}

/// The run's state in `run_dir` as a plain file holds it, if it reads as
/// one.
fn read_state(run_dir: &Path) -> Option<RunState> {
    let path = run_dir.join(RUN_STATE_FILE);
    fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())?;
    RunState::parse(&String::from_utf8_lossy(&fs::read(&path).ok()?)).ok()
}

/// The Git operation half done in the clone (`rebase`, `merge`,
/// `cherry-pick`, `revert`), if any.
fn git_operation(clone: &CloneGit) -> Result<Option<&'static str>> {
    for (name, operation) in [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        let path = git_ok(clone, &["rev-parse", "--git-path", name])?;
        let path = Path::new(path.trim());
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            clone.join(path)
        };
        if path.exists() {
            return Ok(Some(operation));
        }
    }
    Ok(None)
}

/// The uncommitted work of the clone on `head` as a snapshot saves it
/// (its generation left for the caller), and the bytes of its parts: the
/// staged and the unstaged diff, as `git apply` takes them back, and each
/// untracked file Git does not ignore, with its execute bit. An untracked
/// entry that is no regular file, or past a snapshot's limits, ends the
/// work with why ([`CloneWork::refused`]) and none of its parts.
fn read_work(clone: &CloneGit, head: &str) -> Result<(CloneWork, Vec<Vec<u8>>)> {
    clone.refuse_nested()?;
    let diff = |cached: bool| {
        let mut args = vec![
            "diff",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ];
        if cached {
            args.push("--cached");
        }
        git_bytes(clone, &args)
    };
    let mut parts = Vec::new();
    let mut contents = Vec::new();
    for (kind, cached) in [(PartKind::Index, true), (PartKind::Worktree, false)] {
        let bytes = diff(cached)?;
        parts.push(Part::of(kind, None, false, &bytes));
        contents.push(bytes);
    }
    let mut refused = None;
    let mut total: u64 = contents.iter().map(|bytes| bytes.len() as u64).sum();
    let listed = git_ok(clone, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in listed.split('\0').filter(|path| !path.is_empty()) {
        let file = clone.join(path);
        let metadata =
            fs::symlink_metadata(&file).with_context(|| format!("look at {}", file.display()))?;
        if let Some(why) =
            clone_work::refused_entry(path, metadata.is_file(), metadata.len(), total)
        {
            refused = Some(why);
            break;
        }
        let bytes = fs::read(&file).with_context(|| format!("read {}", file.display()))?;
        total += bytes.len() as u64;
        let executable = metadata.permissions().mode() & 0o100 != 0;
        parts.push(Part::of(
            PartKind::Untracked,
            Some(path.to_owned()),
            executable,
            &bytes,
        ));
        contents.push(bytes);
    }
    if refused.is_some() {
        parts.clear();
        contents.clear();
    }
    let work = CloneWork {
        generation: 0,
        head: head.to_owned(),
        git_operation: git_operation(clone)?.map(str::to_owned),
        refused,
        parts,
    };
    Ok((work, contents))
}

/// Apply the restore the control side handed in `run_dir`
/// ([`RestoreRequest`]) to the clone on `branch`, and answer it
/// ([`RestoreResult`]), as [`clone_work::executor_restore`] decides for
/// the clone as it is: nothing for a clone as the snapshot saved it or a
/// restore answered before; the snapshot's head, diffs and untracked files
/// put into the clone the control side made again, and compared with the
/// snapshot once put in; a clone that changed since the park not touched.
/// Nothing while no restore is handed. `Err` (a restore refused, or one
/// that cannot be read) runs no agent.
pub fn restore_clone(clone: &Path, run_dir: &Path, branch: &str) -> Result<()> {
    restore_in(&CloneGit::plain(clone), run_dir, branch)
}

/// [`restore_clone`] with the executor's git of `clone`.
fn restore_in(clone: &CloneGit, run_dir: &Path, branch: &str) -> Result<()> {
    let path = run_dir.join(RUN_RESTORE_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => bail!("{} is not a plain file", path.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("look at {}", path.display())),
    }
    let (request, parts): (RestoreRequest, _) = clone_work::unpack(&fs::read(&path)?)
        .map_err(|why| anyhow::anyhow!("the restore handed to the clone cannot be read: {why}"))?;
    ensure!(
        request.branch == branch,
        "the restore handed is of the branch {}, not the run's {branch}",
        request.branch
    );
    let answered = read_result(run_dir);
    let answer = |outcome: std::result::Result<&str, &str>| {
        write_file(
            run_dir,
            RESTORE_RESULT_FILE,
            &serde_json::to_vec(&RestoreResult {
                snapshot_id: request.snapshot_id.clone(),
                attempt: request.attempt,
                action: outcome.ok().map(str::to_owned),
                failed: outcome.err().map(str::to_owned),
            })?,
        )
    };
    // Answered before: the turns since changed the clone, which is not read.
    if answered
        .as_ref()
        .is_some_and(|answered| answered.answers(&request.snapshot_id, request.attempt))
        && let Some(Ok(_)) = answered.as_ref().map(RestoreResult::outcome)
    {
        return Ok(());
    }
    let current = match clone_state(clone) {
        Ok(current) => current,
        Err(error) => {
            let why = format!("the clone cannot be compared with the snapshot: {error:#}");
            answer(Err(&why))?;
            bail!("the parked run's work was not restored: {why}")
        }
    };
    let operation = git_operation(clone)?;
    match clone_work::executor_restore(&request, answered.as_ref(), &current, operation) {
        ExecutorRestore::Done => Ok(()),
        ExecutorRestore::Intact => answer(Ok("intact")),
        ExecutorRestore::Fill => {
            let filled =
                fill_clone(clone, run_dir, &request, &parts).and_then(|()| {
                    match compare(&request.state(), &clone_state(clone)?) {
                        RestoreCheck::Diverged(why) => {
                            bail!("the clone filled again is not as saved: {why}")
                        }
                        RestoreCheck::Intact | RestoreCheck::Missing => Ok(()),
                    }
                });
            match filled {
                Ok(()) => answer(Ok("recreated")),
                Err(error) => {
                    let why = format!("{error:#}");
                    answer(Err(&why))?;
                    bail!("the parked run's work was not restored: {why}")
                }
            }
        }
        ExecutorRestore::Refuse(why) => {
            if answered
                .as_ref()
                .is_none_or(|answered| !answered.answers(&request.snapshot_id, request.attempt))
            {
                answer(Err(&why))?;
            }
            bail!("the parked run's work was not restored: {why}")
        }
    }
}

/// The clone as a snapshot compares it; `Err` also when its work cannot be
/// read whole (an untracked link, special file or file past the limits),
/// which is not taken for a clean clone.
fn clone_state(clone: &CloneGit) -> Result<crate::domain::replan_snapshot::WorktreeState> {
    let head = git_ok(clone, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let (work, _) = read_work(clone, head.trim())?;
    if let Some(why) = &work.refused {
        bail!("{why}");
    }
    Ok(work.state())
}

/// The executor's answer in `run_dir`, if a plain file holds one.
fn read_result(run_dir: &Path) -> Option<RestoreResult> {
    let path = run_dir.join(RESTORE_RESULT_FILE);
    fs::symlink_metadata(&path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())?;
    serde_json::from_slice(&fs::read(&path).ok()?).ok()
}

/// Put the work of `request` (its parts' bytes `parts`) into the clone at
/// the base: the head from its bundle on the run branch, the staged diff
/// to the index and the worktree, the unstaged one to the worktree and the
/// untracked files with their execute bits.
fn fill_clone(
    clone: &CloneGit,
    run_dir: &Path,
    request: &RestoreRequest,
    parts: &[Vec<u8>],
) -> Result<()> {
    let of = |kind: PartKind| {
        request
            .parts
            .iter()
            .zip(parts)
            .filter(move |(part, _)| part.kind == kind)
    };
    if let Some((_, bundle)) = of(PartKind::Bundle).next() {
        let tmp = run_dir.join(format!("{RUN_RESTORE_FILE}.bundle"));
        remove_if_there(&tmp)?;
        fs::write(&tmp, bundle).with_context(|| format!("write {}", tmp.display()))?;
        let file = tmp.to_string_lossy().into_owned();
        let fetched = git_ok(clone, &["bundle", "list-heads", &file]).and_then(|heads| {
            let reference = heads
                .lines()
                .find_map(|line| line.split_once(' ').map(|(_, name)| name.to_owned()))
                .context("the restore's bundle lists no ref")?;
            git_ok(
                clone,
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--no-write-fetch-head",
                    "--no-recurse-submodules",
                    &file,
                    &format!("+{reference}:refs/dagq/restored"),
                ],
            )
        });
        remove_if_there(&tmp)?;
        fetched?;
    }
    git_ok(
        clone,
        &[
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{}", request.branch),
        ],
    )?;
    git_ok(clone, &["reset", "--quiet", "--hard", &request.head])?;
    for (kind, args) in [
        (PartKind::Index, &["apply", "--index", "--binary", "-"][..]),
        (PartKind::Worktree, &["apply", "--binary", "-"][..]),
    ] {
        for (_, patch) in of(kind).filter(|(_, patch)| !patch.is_empty()) {
            git_with_input(clone, args, patch)?;
        }
    }
    for (part, bytes) in of(PartKind::Untracked) {
        let path = part
            .path
            .as_deref()
            .context("an untracked part names no path")?;
        check_relative(path)?;
        ensure!(
            path.split('/').next() != Some(".git"),
            "an untracked part names {path}, inside the clone's git directory"
        );
        write_in_clone(clone, path, bytes, part.executable)
            .with_context(|| format!("write {path} in the clone"))?;
    }
    Ok(())
}

/// Write `bytes` as the new file `path` (plain names, checked) of the
/// clone, executable or not, through the clone's directory descriptors:
/// each directory on the way is opened, or made, without following a
/// symlink, and the file is made new without following one and has its
/// mode set on its own descriptor, so a link the head or a diff put in
/// the clone never has the write land outside it.
fn write_in_clone(clone: &Path, path: &str, bytes: &[u8], executable: bool) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let open_at =
        |dir: Option<&OwnedFd>, name: &CString, flags: libc::c_int, mode: libc::c_uint| {
            let at = dir.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
            // SAFETY: `name` is NUL-terminated; the descriptor returned is owned.
            let fd = unsafe { libc::openat(at, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
            if fd < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                // SAFETY: `fd` was just opened and is owned by nobody else.
                Ok(unsafe { OwnedFd::from_raw_fd(fd) })
            }
        };
    let directory = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let root = CString::new(clone.as_os_str().as_bytes())?;
    let mut dir =
        open_at(None, &root, directory, 0).with_context(|| format!("open {}", clone.display()))?;
    let mut names: Vec<&str> = path.split('/').collect();
    let file = names.pop().context("no file name")?;
    for name in names {
        let name = CString::new(name)?;
        let next = match open_at(Some(&dir), &name, directory, 0) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // SAFETY: `dir` is open and `name` NUL-terminated.
                if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                }
                open_at(Some(&dir), &name, directory, 0)?
            }
            opened => opened?,
        };
        dir = next;
    }
    let name = CString::new(file)?;
    let made = open_at(
        Some(&dir),
        &name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        0o600,
    )?;
    let mut made = fs::File::from(made);
    made.write_all(bytes)?;
    made.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))?;
    Ok(())
}

/// Write `bytes` as `name` in `run_dir`, to a temporary file renamed into
/// place.
fn write_file(run_dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp = run_dir.join(format!("{name}.tmp"));
    remove_if_there(&tmp)?;
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, run_dir.join(name))
        .with_context(|| format!("rename {} into place", tmp.display()))
}

/// Write `state` as the run's state in `run_dir`, to a temporary file
/// renamed into place.
fn write_state(run_dir: &Path, state: &RunState) -> Result<()> {
    write_file(run_dir, RUN_STATE_FILE, &serde_json::to_vec(state)?)
}

/// Remove `path` (never what a symlink there points at), if it is there.
fn remove_if_there(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// A clone on the run branch at `base`, and its run directory.
    fn clone_at_base() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
        let run_dir = dir.path().join("run");
        fs::create_dir_all(&clone).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        run_git(&clone, &["init", "-q", "-b", "dagq/r1"]);
        fs::write(clone.join("a"), "a\n").unwrap();
        run_git(&clone, &["add", "a"]);
        run_git(&clone, &["commit", "-qm", "base"]);
        let base = run_git(&clone, &["rev-parse", "HEAD"]);
        (dir, clone, run_dir, base)
    }

    /// A turn that made no commit leaves no bundle (one left before goes)
    /// and a state at the base; one whose agent left a rebase stopped and
    /// a detached HEAD says so, with the run branch's bundle of its
    /// commits only.
    #[test]
    fn the_turn_s_end_writes_the_branch_s_bundle_and_the_state_as_the_clone_has_them() {
        let (_dir, clone, run_dir, base) = clone_at_base();
        fs::write(run_dir.join(RUN_BUNDLE_FILE), "stale").unwrap();
        let state = write_turn_end(&clone, &run_dir, &base, "dagq/r1").unwrap();
        assert!(!run_dir.join(RUN_BUNDLE_FILE).exists());
        assert_eq!(state.head, base);
        assert_eq!(state.branch.as_deref(), Some("refs/heads/dagq/r1"));
        assert_eq!(state.status, "");
        assert!(!state.rebase_in_progress);
        let read =
            RunState::parse(&fs::read_to_string(run_dir.join(RUN_STATE_FILE)).unwrap()).unwrap();
        assert_eq!(read, state);

        fs::write(clone.join("b"), "b\n").unwrap();
        run_git(&clone, &["add", "b"]);
        run_git(&clone, &["commit", "-qm", "work"]);
        let head = run_git(&clone, &["rev-parse", "HEAD"]);
        run_git(&clone, &["checkout", "-q", "--detach"]);
        fs::create_dir_all(clone.join(".git/rebase-merge")).unwrap();
        fs::write(clone.join("c"), "c\n").unwrap();
        let state = write_turn_end(&clone, &run_dir, &base, "dagq/r1").unwrap();
        assert_eq!(state.branch, None);
        assert_eq!(state.head, head);
        assert_eq!(state.status, "?? c\n");
        assert!(state.rebase_in_progress);
        let bundle = run_dir.join(RUN_BUNDLE_FILE).to_string_lossy().into_owned();
        assert_eq!(
            run_git(&clone, &["bundle", "list-heads", &bundle]),
            format!("{head} refs/heads/dagq/r1")
        );
    }

    /// A program `path` that writes `mark` when it runs, and then what
    /// `then` says.
    fn marking(path: &Path, mark: &Path, then: &str) {
        fs::write(
            path,
            format!("#!/bin/sh\necho ran >> '{}'\n{then}\n", mark.display()),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A clone whose configuration names a fsmonitor, a hooks directory
    /// with a `reference-transaction` hook and a clean filter its tracked
    /// `.gitattributes` uses, each marking the file it is named after, and
    /// a tracked file changed since its commit so that `status` and `diff`
    /// run the filter; the marks' directory.
    fn planted_clone() -> (tempfile::TempDir, PathBuf, PathBuf, String, PathBuf) {
        let (dir, clone, run_dir, base) = clone_at_base();
        let marks = dir.path().join("marks");
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(&marks).unwrap();
        fs::create_dir_all(&hooks).unwrap();
        let fsmonitor = dir.path().join("fsmonitor");
        marking(&fsmonitor, &marks.join("fsmonitor"), "exit 1");
        marking(
            &hooks.join("reference-transaction"),
            &marks.join("hook"),
            "exit 0",
        );
        let filter = dir.path().join("clean");
        marking(&filter, &marks.join("filter"), "cat");
        fs::write(clone.join(".gitattributes"), "*.txt filter=planted\n").unwrap();
        fs::write(clone.join("f.txt"), "f\n").unwrap();
        run_git(&clone, &["add", ".gitattributes", "f.txt"]);
        run_git(&clone, &["commit", "-qm", "planted"]);
        for (key, value) in [
            ("core.fsmonitor", fsmonitor.to_str().unwrap()),
            ("core.hooksPath", hooks.to_str().unwrap()),
            ("filter.planted.clean", filter.to_str().unwrap()),
        ] {
            run_git(&clone, &["config", key, value]);
        }
        fs::write(clone.join("f.txt"), "changed\n").unwrap();
        (dir, clone, run_dir, base, marks)
    }

    fn marks_of(marks: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(marks)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    /// The facts-only turn's git runs nothing the worker left in the
    /// clone's configuration (no fsmonitor, no hook, no driver), while the
    /// executor's plain git of a worker's turn runs them as git would: it
    /// still writes the clone's facts, the changed file uncommitted.
    #[test]
    fn a_facts_only_turn_runs_no_fsmonitor_hook_or_driver_the_worker_left_in_the_clone() {
        let (_dir, clone, run_dir, base, marks) = planted_clone();
        let guarded = CloneGit::guarded(&clone).unwrap();
        git_ok(&guarded, &["update-ref", "refs/dagq/probe", "HEAD"]).unwrap();
        run_facts_turn(&clone, &run_dir, &base, "dagq/r1").unwrap();
        assert!(marks_of(&marks).is_empty(), "{:?}", marks_of(&marks));
        let state = run_state(&run_dir);
        assert_eq!(state.turn_ended, Some(true));
        assert!(state.status.contains("f.txt"), "{}", state.status);

        // The same git without the guard runs each of them.
        let plain = CloneGit::plain(&clone);
        git_ok(&plain, &["update-ref", "refs/dagq/probe", "HEAD~1"]).unwrap();
        write_turn_end(&clone, &run_dir, &base, "dagq/r1").unwrap();
        assert_eq!(marks_of(&marks), ["filter", "fsmonitor", "hook"]);
    }

    /// A clone with a populated submodule is refused by the facts-only
    /// turn before any `status` or `diff` runs git in the nested
    /// repository, whose own configuration defines a clean filter here:
    /// no facts are written, and the filter does not run.
    #[test]
    fn a_facts_only_turn_refuses_a_clone_with_a_populated_submodule() {
        let (dir, clone, run_dir, base) = clone_at_base();
        let nested = clone.join("sub");
        fs::create_dir_all(&nested).unwrap();
        run_git(&nested, &["init", "-q"]);
        fs::write(nested.join(".gitattributes"), "* filter=nested\n").unwrap();
        fs::write(nested.join("n.txt"), "n\n").unwrap();
        run_git(&nested, &["add", "."]);
        run_git(&nested, &["commit", "-qm", "nested"]);
        let marks = dir.path().join("marks");
        fs::create_dir_all(&marks).unwrap();
        let filter = dir.path().join("clean");
        marking(&filter, &marks.join("filter"), "cat");
        run_git(
            &nested,
            &["config", "filter.nested.clean", filter.to_str().unwrap()],
        );
        fs::write(nested.join("n.txt"), "changed\n").unwrap();
        run_git(&clone, &["add", "sub"]);
        run_git(&clone, &["commit", "-qm", "gitlink"]);
        let refused = run_facts_turn(&clone, &run_dir, &base, "dagq/r1").unwrap_err();
        assert!(
            format!("{refused:#}").contains("populated submodule sub"),
            "{refused:#}"
        );
        assert!(marks_of(&marks).is_empty(), "{:?}", marks_of(&marks));
        assert!(!run_dir.join(RUN_STATE_FILE).exists());
    }

    fn run_state(run_dir: &Path) -> RunState {
        RunState::parse(&fs::read_to_string(run_dir.join(RUN_STATE_FILE)).unwrap()).unwrap()
    }

    /// The overrides of a facts-only turn's git are the control side's
    /// guard and no fsmonitor.
    #[test]
    fn a_facts_only_turn_s_git_has_no_hooks_no_fsmonitor_and_no_driver() {
        let overrides = facts_overrides("core.fsmonitor\0filter.planted.clean\0").unwrap();
        for (key, value) in [
            ("core.hooksPath", "/dev/null"),
            ("core.fsmonitor", "false"),
            ("filter.planted.clean", ""),
            ("filter.planted.required", "false"),
        ] {
            assert!(
                overrides.contains(&(key.to_owned(), value.to_owned())),
                "{key}: {overrides:?}"
            );
        }
    }

    /// A restored untracked file is written inside the clone only: its
    /// directories are made there, and a symlink on the way or in its place
    /// (one the head or a diff put there, pointing out of the clone) is
    /// refused, leaving what it points at as it was.
    #[test]
    fn an_untracked_file_is_restored_inside_the_clone_and_follows_no_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let clone = dir.path().join("clone");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&clone).unwrap();
        fs::create_dir_all(&outside).unwrap();
        write_in_clone(&clone, "a/b/run.sh", b"#!/bin/sh\n", true).unwrap();
        let made = clone.join("a/b/run.sh");
        assert_eq!(fs::read(&made).unwrap(), b"#!/bin/sh\n");
        assert_eq!(
            fs::metadata(&made).unwrap().permissions().mode() & 0o777,
            0o755
        );
        write_in_clone(&clone, "notes.txt", b"n", false).unwrap();
        assert_eq!(
            fs::metadata(clone.join("notes.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        std::os::unix::fs::symlink(&outside, clone.join("link")).unwrap();
        assert!(write_in_clone(&clone, "link/evil.txt", b"x", false).is_err());
        fs::write(outside.join("kept.txt"), b"kept").unwrap();
        std::os::unix::fs::symlink(outside.join("kept.txt"), clone.join("file")).unwrap();
        assert!(write_in_clone(&clone, "file", b"x", false).is_err());
        assert_eq!(fs::read(outside.join("kept.txt")).unwrap(), b"kept");
        assert!(!outside.join("evil.txt").exists());
    }

    /// Main's bundle goes into the clone's main ref; none is nothing, and
    /// one of two refs or a symlink in its place is refused.
    #[test]
    fn main_s_bundle_is_fetched_into_the_clone_s_main_ref_only_as_one_plain_branch() {
        let (dir, clone, run_dir, base) = clone_at_base();
        assert!(!import_main(&clone, &run_dir).unwrap());
        let main = dir.path().join("main");
        run_git(
            dir.path(),
            &["clone", "-q", clone.to_str().unwrap(), "main"],
        );
        run_git(&main, &["checkout", "-q", "-b", "main"]);
        fs::write(main.join("m"), "m\n").unwrap();
        run_git(&main, &["add", "m"]);
        run_git(&main, &["commit", "-qm", "main"]);
        let moved = run_git(&main, &["rev-parse", "HEAD"]);
        let bundle = run_dir.join(MAIN_BUNDLE_FILE);
        let path = bundle.to_string_lossy().into_owned();
        run_git(
            &main,
            &[
                "bundle",
                "create",
                "-q",
                &path,
                "refs/heads/main",
                &format!("^{base}"),
            ],
        );
        assert!(import_main(&clone, &run_dir).unwrap());
        assert_eq!(run_git(&clone, &["rev-parse", CLONE_MAIN_REF]), moved);
        assert!(!clone.join(".git/FETCH_HEAD").exists());

        run_git(&main, &["branch", "other"]);
        run_git(
            &main,
            &[
                "bundle",
                "create",
                "-q",
                &path,
                "refs/heads/main",
                "refs/heads/other",
                &format!("^{base}"),
            ],
        );
        assert!(import_main(&clone, &run_dir).is_err());
        fs::remove_file(&bundle).unwrap();
        std::os::unix::fs::symlink(clone.join("a"), &bundle).unwrap();
        assert!(import_main(&clone, &run_dir).is_err());
    }
}
