//! `dagq relay turn`: what a worker's turn runs inside its container
//! ([Container runtime], ADR-t2179-1 decisions 2 and 6, ADR-t2114-2
//! decision 1, ADR-t2114-3 decision 3). It is the only place that runs git
//! in a container run's clone, beside the worker itself: the control side
//! runs none there.
//!
//! In order: main's bundle the control side left in the run directory is
//! fetched into the clone's [`CLONE_MAIN_REF`]; the forwarder starts on the
//! container's loopback; the agent runs in the clone with [`turn_env`],
//! made from the run token's file and the run; and whatever the agent did,
//! the run branch's bundle and the run's state are written to the run
//! directory, each to a temporary file renamed into place. Its exit code
//! is the agent's. It writes nothing to its standard output, which is the
//! agent's (the turn's output the wrapper reads).
//!
//! [Container runtime]: ../../docs/design/container-runtime.md

use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
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
use crate::domain::queue_service::CREDENTIAL_FILE_ENV;
use crate::domain::run_clone::{
    CLONE_MAIN_REF, MAIN_BUNDLE_FILE, RUN_BUNDLE_FILE, RUN_STATE_FILE, RunState,
};
use crate::domain::run_container::{TOKEN_FILE, turn_env, verification_env};

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
/// one). An error before the agent starts (no run, no token) runs no agent
/// and writes no state.
pub fn run_turn(options: &TurnOptions, env: impl Fn(&str) -> Option<String>) -> Result<i32> {
    let run = env(RUN_ID_ENV).with_context(|| format!("{RUN_ID_ENV} names no run"))?;
    let token_file =
        env(CREDENTIAL_FILE_ENV).map_or_else(|| options.run_dir.join(TOKEN_FILE), PathBuf::from);
    let token = fs::read_to_string(&token_file)
        .with_context(|| format!("read the run token's file {}", token_file.display()))?;
    let token = token.trim();
    ensure!(!token.is_empty(), "{} holds no token", token_file.display());
    // A bundle that cannot be fetched leaves the ref where it was: the
    // agent's rebase then finds main as it was, and the control side asks
    // again; the turn itself goes on.
    if let Err(error) = import_main(&options.clone, &options.run_dir) {
        eprintln!("dagq: main's bundle was not fetched into {CLONE_MAIN_REF}: {error:#}");
    }
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
        .stdin(Stdio::null())
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
        eprintln!("dagq: the run's bundle and state were not written: {error:#}");
    }
    let status = status?;
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
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

/// Git in the clone, with the configuration git reads there: the worker's
/// own, in the worker's container.
fn git(clone: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(clone)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))
}

fn git_ok(clone: &Path, args: &[&str]) -> Result<String> {
    let output = git(clone, args)?;
    if !output.status.success() {
        bail!(
            "git {} exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetch main's bundle of `run_dir` into the clone's [`CLONE_MAIN_REF`]
/// (ADR-t2179-1 decision 6): its one ref, forced, without tags and
/// without a `FETCH_HEAD`. Nothing while there is no bundle (main is still
/// the base) or the path holds anything but a plain file. `true` when it
/// was fetched.
pub fn import_main(clone: &Path, run_dir: &Path) -> Result<bool> {
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

/// Write the run's facts of the turn's end in `run_dir` (ADR-t2179-1
/// decision 2): the run branch's bundle on top of `base`, removed while
/// the branch has no new commit, then the run's state. Each is written to
/// a temporary file and renamed, the bundle first.
pub fn write_turn_end(clone: &Path, run_dir: &Path, base: &str, branch: &str) -> Result<RunState> {
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
    let state = RunState {
        branch: pointed,
        head,
        status,
        rebase_in_progress,
    };
    let tmp = run_dir.join(format!("{RUN_STATE_FILE}.tmp"));
    remove_if_there(&tmp)?;
    fs::write(&tmp, serde_json::to_vec(&state)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, run_dir.join(RUN_STATE_FILE))
        .with_context(|| format!("rename {} into place", tmp.display()))?;
    Ok(state)
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
