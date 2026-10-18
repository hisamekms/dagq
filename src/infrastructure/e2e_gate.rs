//! The e2e gate on this machine (ADR-t963-1 decision 1): before a build of
//! dagq's source is put in place, `cargo test --locked --test e2e --
//! --ignored` runs every e2e in its checkout with the `[run.env]` of the
//! repository and a `TMPDIR` of its own under the gate's scratch directory;
//! none is left out for a tool the host lacks (ADR-t2159-1 decision 7).
//! Past its timeout its process group is stopped. After it, the processes
//! whose command line names that `TMPDIR` are stopped and the directory is
//! removed.
//!
//! The e2e runs code a worker wrote (`build.rs`, proc-macros, tests) on the
//! host, so it is not given the environment of the process that starts it
//! (the supervisor, or the shell of `install`): its environment is cleared
//! and only [`PASSED_ENV`] and [`PASSED_PREFIXES`] of it are given back,
//! never a credential ([`credential`]), through the narrowing the review's program job shares
//! ([`super::passed_env`]), with the `[run.env]` and the gate's
//! own over them (ADR-t1233-2's Consequences). The log names what was
//! passed, never the values.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::application::install::{E2eOutcome, E2eRerun, E2eSettings};
use crate::domain::e2e_quarantine::{self, QuarantineFile};
use crate::infrastructure::adapters::unpiped_output_within;
use crate::infrastructure::passed_env::PassedEnv;
pub use crate::infrastructure::passed_env::credential;

/// How long the `ps` of the cleanup may take.
const CALL_LIMIT: Duration = Duration::from_secs(20);

/// How long a stopped e2e (or a process it left) is given to end on
/// SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The variables of the starting process's environment the e2e is given
/// by name: what cargo, rustup, Git and a shell need to find their tools,
/// home, config (`XDG_CONFIG_HOME`) and locale, and the e2e's own opt-in
/// (`DAGQ_E2E_WINDOWS`). `TMPDIR`, `CARGO_TARGET_DIR` and `DAGQ_E2E_RERUN`
/// are the gate's own.
///
/// No key of `dagq.toml` adds a name: a binary that does not know such a
/// key could not read the file, so one is added only after the fixed
/// binary reads it. A value that is not a credential can be put in the
/// repository's `[run.env]` meanwhile, which is given as written.
///
/// A fixed binary whose list lacks a name a tool needs keeps a run's e2e
/// from starting (the inbox's `check the e2e host` stays) or fails it for
/// a missing tool, and can fail the automatic update's gate too: the
/// log's `== the env passed` line (the names given) and the e2e's output
/// show which name was missing, and the way back is
/// `dagq install --rollback` and a task that adds the name.
pub const PASSED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TZ",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "SDKROOT",
    "DEVELOPER_DIR",
    "DAGQ_E2E_WINDOWS",
];

/// The prefixes of the variables given too: the locale (`LC_*`).
pub const PASSED_PREFIXES: &[&str] = &["LC_"];

/// What the e2e is given of the starting process's environment: the
/// common narrowing (ADR-t1895-2 decision 6) with [`PASSED_ENV`] and
/// [`PASSED_PREFIXES`], and no credential.
const GATE_ENV: PassedEnv<'static> = PassedEnv {
    names: PASSED_ENV,
    prefixes: PASSED_PREFIXES,
    exceptions: &[],
};

/// Whether the starting process's `name` is given to the e2e.
pub fn passed(name: &str) -> bool {
    GATE_ENV.passes(name)
}

/// What of `inherited` the e2e is given, as [`passed`] says; a name that is
/// not UTF-8 is not.
fn passed_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> BTreeMap<OsString, OsString> {
    GATE_ENV.filter(inherited)
}

/// Run the e2e of `checkout` as the module says; an error is an e2e that
/// could not start.
pub fn run(
    checkout: &Path,
    target_dir: Option<&Path>,
    settings: &E2eSettings,
) -> Result<E2eOutcome> {
    // Only looked at: the supervisor starts the server (ADR-t2086-1). The
    // guard is this `dagq`.
    let sccache = super::sccache::SystemSccache {
        dagq: std::env::current_exe().ok(),
        ..super::sccache::SystemSccache::new(&settings.scratch, Duration::ZERO)
    };
    run_inheriting(
        checkout,
        target_dir,
        settings,
        std::env::vars_os(),
        &sccache,
    )
}

/// [`run`], with `inherited` as the starting process's environment and
/// `sccache` to look at the server of its `[run.env]` with.
fn run_inheriting(
    checkout: &Path,
    target_dir: Option<&Path>,
    settings: &E2eSettings,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    sccache: &dyn crate::application::SccacheServer,
) -> Result<E2eOutcome> {
    let inherited = passed_env(inherited);
    let mut env = match &settings.run_env_root {
        Some(root) => super::run_env::load_run_env(
            root,
            settings.queue_dir.as_deref().unwrap_or(&settings.scratch),
            &settings.scratch,
        )
        .context("read the [run.env] the e2e runs with")?,
        None => Vec::new(),
    };
    env.extend(settings.git_env.iter().cloned());
    // The host's one e2e at a time (ADR-t1233-2 decision 4), held until
    // this returns.
    let waiting = Instant::now();
    let _lock = settings.lock.as_deref().map(hold_lock).transpose()?;
    let lock_wait_secs = waiting.elapsed().as_secs();
    fs::create_dir_all(&settings.scratch)
        .with_context(|| format!("create {}", settings.scratch.display()))?;
    // The e2e and its rerun may not start the sccache server
    // (ADR-t2086-1): looked at once its turn came, the guard beside its
    // fixtures.
    let sccache_wrapper_removed = guard_e2e(sccache, &settings.scratch, &mut env);
    // What an earlier gate left when it was stopped before its cleanup,
    // this process's earlier gates' included (the supervisor runs its
    // runs' e2e in-process); a
    // gate still running (an install next to the automatic update's job,
    // another of this process) keeps its own.
    for earlier in subdirectories(&settings.scratch) {
        if finished(&earlier) {
            clean_up(&earlier);
        }
    }
    let first = pass(
        checkout,
        target_dir,
        settings,
        &inherited,
        &env,
        &Pass {
            log: &settings.log,
            rerun: None,
        },
    )?;
    let passed = !first.timed_out && first.success;
    let failed = failed_tests(&first.output);
    let rerun = to_rerun(&first.output, passed, first.timed_out).map(|failed| {
        let rerun_log = settings.rerun_log();
        match pass(
            checkout,
            target_dir,
            settings,
            &inherited,
            &env,
            &Pass {
                log: &rerun_log,
                rerun: Some(&failed),
            },
        ) {
            Ok(again) => {
                let still = still_failing(&failed, &again.output, again.timed_out);
                E2eRerun {
                    tests: failed.clone(),
                    failed: still,
                    timed_out: again.timed_out,
                    secs: again.secs,
                    cleanup: again.cleanup,
                    error: None,
                }
            }
            Err(error) => E2eRerun {
                tests: failed.clone(),
                failed: failed.clone(),
                error: Some(format!("{error:#}")),
                ..Default::default()
            },
        }
    });
    Ok(E2eOutcome {
        passed,
        timed_out: first.timed_out,
        failed_tests: failed,
        secs: first.secs,
        cleanup: first.cleanup,
        rerun,
        quarantine: read_quarantine(checkout),
        lock_wait_secs,
        sccache_wrapper_removed,
    })
}

/// The failed tests to run once more by name, as the rerun of the
/// landing's verification (ADR-t1165-1), of a first run whose output is
/// `output`: `None` when it `passed`, was stopped past its timeout, named
/// no failed test, or ended before libtest's summary (a `within` past its
/// limit exits the binary, cutting off tests that then name no result).
fn to_rerun(output: &str, passed: bool, timed_out: bool) -> Option<Vec<String>> {
    let summarized = output
        .lines()
        .any(|line| line.trim_start().starts_with("test result: FAILED"));
    let failed = failed_tests(output);
    (!passed && !timed_out && summarized && !failed.is_empty()).then_some(failed)
}

/// Which of the `rerun` tests failed their rerun, whose output is `output`:
/// only a test it reports `ok` passed it, so one it failed or never
/// reported (cut off) failed it, and all of them when it ran past its
/// timeout.
fn still_failing(rerun: &[String], output: &str, timed_out: bool) -> Vec<String> {
    let passed = passed_tests(output);
    rerun
        .iter()
        .filter(|test| timed_out || !passed.contains(test))
        .cloned()
        .collect()
}

/// Put into `env`, the e2e's `[run.env]`, what the look at its sccache
/// server found ([`crate::application::sccache::look`], the guard made in
/// `dir`), and return the port and why when the e2e runs without
/// `RUSTC_WRAPPER`, for its caller to record as `sccache_wrapper_removed`.
fn guard_e2e(
    sccache: &dyn crate::application::SccacheServer,
    dir: &Path,
    env: &mut Vec<(String, String)>,
) -> Option<(u16, String)> {
    let target = crate::domain::sccache::SccacheTarget::of_pairs(env)?;
    let look = crate::application::sccache::look(sccache, &target, dir);
    look.apply(env);
    look.removed().map(|(port, why)| (port, why.to_owned()))
}

/// Wait for the host's e2e lock at `path` and hold it while the file stays
/// open (`flock`, released when it closes or the process ends).
fn hold_lock(path: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns; it blocks until the
    // lock is free and is released when the file closes.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("lock {}", path.display()));
    }
    Ok(file)
}

/// The marks of `checkout`'s `.config/e2e-quarantine.toml` (ADR-t1165-1).
pub fn read_quarantine(checkout: &Path) -> QuarantineFile {
    match fs::read_to_string(checkout.join(e2e_quarantine::FILE)) {
        Ok(text) => QuarantineFile::of(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => QuarantineFile::Absent,
        Err(error) => QuarantineFile::Unreadable(error.to_string()),
    }
}

/// One run of the e2e of the gate: where its output goes and, for the
/// rerun, the tests to run by name.
struct Pass<'a> {
    log: &'a Path,
    rerun: Option<&'a [String]>,
}

/// How one run went.
struct Ran {
    success: bool,
    timed_out: bool,
    /// What it appended to its log.
    output: String,
    secs: u64,
    cleanup: Value,
}

/// Run the e2e once as `how` says in a `TMPDIR` of its own, within the
/// timeout, and clean up after it; an error is one that could not start.
fn pass(
    checkout: &Path,
    target_dir: Option<&Path>,
    settings: &E2eSettings,
    inherited: &BTreeMap<OsString, OsString>,
    env: &[(String, String)],
    how: &Pass,
) -> Result<Ran> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    // The rerun's own, beside the first's if that could not be removed;
    // both end in the gate's pid, which the next gate reads.
    let kind = if how.rerun.is_some() { ".rerun" } else { "" };
    let root = settings
        .scratch
        .join(format!("{stamp}{kind}-{}", std::process::id()));
    let _in_use = InUse::take(&root);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    if let Some(dir) = how.log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(how.log)
        .with_context(|| format!("open {}", how.log.display()))?;
    let start = log.seek(SeekFrom::End(0)).unwrap_or_default();
    match how.rerun {
        None => {
            let _ = writeln!(
                log,
                "== the e2e of {} (TMPDIR {})",
                checkout.display(),
                root.display()
            );
        }
        Some(tests) => {
            let _ = writeln!(
                log,
                "== the rerun by name of the e2e tests that failed in {}: {} (TMPDIR {})",
                checkout.display(),
                tests.join(" "),
                root.display()
            );
        }
    }
    let given = given_env(inherited, env, &root, target_dir, how.rerun);
    let _ = writeln!(
        log,
        "== the env passed (names only): {}",
        given
            .keys()
            .map(|name| name.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut command = e2e_command(settings, how.rerun);
    command
        .current_dir(checkout)
        .env_clear()
        .envs(&given)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .process_group(0);
    let started = Instant::now();
    let spawned = command.spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            clean_up(&root);
            return Err(error).context("start the e2e");
        }
    };
    let deadline = started + settings.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                break stop_group(&mut child, STOP_GRACE);
            }
            Ok(None) => thread::sleep(Duration::from_millis(200)),
            Err(_) => {
                stop_group(&mut child, STOP_GRACE);
                break None;
            }
        }
    };
    // What the e2e left running in its group once it ended (a stub's
    // `sleep`) goes with it.
    signal_group(child.id(), libc::SIGKILL);
    let secs = started.elapsed().as_secs();
    let mut output = String::new();
    if let Ok(mut file) = fs::File::open(how.log)
        && file.seek(SeekFrom::Start(start)).is_ok()
    {
        let _ = file.read_to_string(&mut output);
    }
    let cleanup = clean_up(&root);
    let _ = writeln!(
        log,
        "== the {} {} after {secs}s; cleanup: {cleanup}",
        if how.rerun.is_some() { "rerun" } else { "e2e" },
        if timed_out {
            "timed out".to_owned()
        } else {
            status.map_or("could not be waited for".to_owned(), |s| {
                format!("exited with {s}")
            })
        }
    );
    Ok(Ran {
        success: status.is_some_and(|status| status.success()),
        timed_out,
        output,
        secs,
        cleanup,
    })
}

/// The env naming the tests the rerun runs by name (space separated), for
/// a `command` in place of cargo's (ADR-t1165-1).
pub const RERUN_ENV: &str = "DAGQ_E2E_RERUN";

/// What one run of the e2e is given: the passed of the starting
/// process's environment (`inherited`), the `[run.env]` (`env`) over them
/// and the gate's own over both (its `TMPDIR` `root`, the
/// `CARGO_TARGET_DIR`, and for the rerun its tests).
fn given_env(
    inherited: &BTreeMap<OsString, OsString>,
    env: &[(String, String)],
    root: &Path,
    target_dir: Option<&Path>,
    rerun: Option<&[String]>,
) -> BTreeMap<OsString, OsString> {
    let mut given = inherited.clone();
    given.extend(env.iter().map(|(key, value)| (key.into(), value.into())));
    // The e2e's result is the runtime's own record (ADR-t2589-1 decision 2).
    given.remove(std::ffi::OsStr::new(crate::domain::local_checks::DIR_ENV));
    given.insert("TMPDIR".into(), root.into());
    if let Some(dir) = target_dir {
        given.insert("CARGO_TARGET_DIR".into(), dir.into());
    }
    if let Some(tests) = rerun {
        given.insert(RERUN_ENV.into(), tests.join(" ").into());
    }
    given
}

/// The command of one run of the e2e: `settings`' command in a shell, or
/// cargo's running every e2e, the `rerun`'s tests by whole name.
fn e2e_command(settings: &E2eSettings, rerun: Option<&[String]>) -> Command {
    match &settings.command {
        Some(command) => {
            let mut shell = Command::new("/bin/sh");
            shell.args(["-c", command]);
            shell
        }
        None => {
            let mut cargo =
                Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            cargo.args(["test", "--locked", "--test", "e2e", "--", "--ignored"]);
            if let Some(tests) = rerun {
                cargo.arg("--exact").args(tests);
            }
            cargo
        }
    }
}

/// The tests a libtest output names as failed (`test <name> ... FAILED`),
/// each once, in order.
pub fn failed_tests(output: &str) -> Vec<String> {
    let mut failed: Vec<String> = Vec::new();
    for line in output.lines() {
        if let Some(name) = line
            .trim()
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
            && !failed.iter().any(|known| known == name)
        {
            failed.push(name.to_owned());
        }
    }
    failed
}

/// The tests a libtest output reports as passed (`test <name> ... ok`).
pub fn passed_tests(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("test ")
                .and_then(|rest| rest.strip_suffix(" ... ok"))
                .map(str::to_owned)
        })
        .collect()
}

/// Run `command` to its end within [`CALL_LIMIT`], its output captured;
/// past it the command is killed and it is an error.
fn bounded(command: &mut Command) -> Result<Output> {
    bounded_within(command, CALL_LIMIT)
}

/// [`bounded`] with `limit`. The output is taken in unlinked files, not
/// pipes: a pipe's write end inherited by a process the supervisor spawns
/// meanwhile would hold a read to its end up to the limit (task 1273).
fn bounded_within(command: &mut Command, limit: Duration) -> Result<Output> {
    match unpiped_output_within(command, limit)? {
        Some(output) => Ok(output),
        None => bail!("it did not end within {}s", limit.as_secs()),
    }
}

fn signal(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: kill(2) has no memory preconditions.
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: as above; signal 0 only checks.
    i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
}

/// Send `signal` to the process group `leader` led; a group that is gone
/// makes it fail harmlessly.
fn signal_group(leader: u32, signal: i32) {
    if let Ok(group) = i32::try_from(leader) {
        // SAFETY: kill(2) on the group the e2e leads.
        unsafe {
            libc::kill(-group, signal);
        }
    }
}

/// Stop the process group `leader` leads: SIGTERM, then SIGKILL to the
/// group once the leader has ended or `grace` has passed. The leader is
/// reaped as it is watched, so one that ends on SIGTERM is not mistaken for
/// a live one while it is a zombie; it is waited for after the SIGKILL when
/// it ignored the SIGTERM. The rest of the group gets no grace of its
/// own: what is left once the leader ends is killed. Its exit status is returned when it could be
/// read.
fn stop_group(leader: &mut Child, grace: Duration) -> Option<ExitStatus> {
    signal_group(leader.id(), libc::SIGTERM);
    let deadline = Instant::now() + grace;
    let mut ended = None;
    while Instant::now() < deadline {
        match leader.try_wait() {
            Ok(Some(status)) => {
                ended = Some(status);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    signal_group(leader.id(), libc::SIGKILL);
    ended.or_else(|| leader.wait().ok())
}

/// The roots of this process's gates that are running, which a gate of the
/// same process does not clean up.
static IN_USE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A root of this process's gate in use while this lives.
struct InUse(PathBuf);

impl InUse {
    fn take(root: &Path) -> Self {
        IN_USE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(root.to_path_buf());
        Self(root.to_path_buf())
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut in_use = IN_USE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = in_use.iter().position(|root| *root == self.0) {
            in_use.swap_remove(at);
        }
    }
}

/// Whether the gate that made `dir` is done with it: its process is gone,
/// or it is this process and no gate of it runs there.
fn finished(dir: &Path) -> bool {
    match owner(dir) {
        Some(pid) if pid == std::process::id() => !IN_USE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|root| root == dir),
        Some(pid) => !alive(pid),
        None => true,
    }
}

/// The pid of the gate that made `dir` (`<unix time>-<pid>`).
fn owner(dir: &Path) -> Option<u32> {
    dir.file_name()?.to_str()?.rsplit_once('-')?.1.parse().ok()
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

/// Clean up what the e2e left under `root`: stop the processes inside it,
/// then remove the directory; it is kept, with why, when it cannot be.
/// The value: `dir` the root, `processes` the pids stopped, `removed`
/// whether the root is gone, `errors` why not.
pub fn clean_up(root: &Path) -> Value {
    let stopped = stop_processes_inside(root);
    let mut errors = Vec::new();
    let removed = match fs::remove_dir_all(root) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            errors.push(format!("remove {}: {error}", root.display()));
            false
        }
    };
    json!({
        "dir": root,
        "processes": stopped,
        "removed": removed,
        "errors": errors,
    })
}

/// Stop the processes an argument of whose command line is a path inside
/// `root` (the e2e's supervisors and stub agents live on after their test
/// when it was stopped); their pids.
fn stop_processes_inside(root: &Path) -> Vec<u32> {
    let mut forms = vec![root.to_path_buf()];
    if let Ok(real) = root.canonicalize()
        && real != root
    {
        forms.push(real);
    }
    let Ok(output) = bounded(Command::new("ps").args(["-axww", "-o", "pid=,command="])) else {
        return Vec::new();
    };
    let me = std::process::id();
    let victims: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse::<u32>().ok()?;
            let inside =
                words.any(|word| forms.iter().any(|form| Path::new(word).starts_with(form)));
            (inside && pid != me).then_some(pid)
        })
        .collect();
    for pid in &victims {
        signal(*pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while victims.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    for pid in victims.iter().filter(|pid| alive(**pid)) {
        signal(*pid, libc::SIGKILL);
    }
    victims
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process holding the command's output open (as one the supervisor
    /// spawns at the same moment can inherit a pipe's write end, task
    /// 1273) does not hold the call past the command's own exit; with
    /// pipes it waited for the holder up to the limit and failed.
    #[test]
    fn bounded_returns_at_the_exit_though_another_process_holds_the_output() {
        let started = Instant::now();
        let output = bounded_within(
            Command::new("/bin/sh").args(["-c", "/bin/sleep 60 & echo $!; echo err >&2"]),
            Duration::from_secs(60),
        )
        .unwrap();
        let held = started.elapsed();
        let holder: u32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        signal(holder, libc::SIGKILL);
        assert!(output.status.success());
        assert_eq!(output.stderr, b"err\n");
        assert!(held < Duration::from_secs(30), "held for {held:?}");
    }

    #[test]
    fn bounded_kills_a_command_past_its_limit() {
        let started = Instant::now();
        let error = bounded_within(
            Command::new("/bin/sh").args(["-c", "exec /bin/sleep 60"]),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "it did not end within 0s");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn the_failed_tests_are_read_from_the_output_once_each() {
        let output = "running 3 tests\ntest a::one ... ok\ntest a::two ... FAILED\n\
test b ... FAILED\nfailures:\n    a::two\ntest a::two ... FAILED\n";
        assert_eq!(failed_tests(output), ["a::two", "b"]);
        assert!(failed_tests("test a ... ok\n").is_empty());
    }

    /// A `sh -c script` in a process group of its own, as the gate starts
    /// the e2e, until [`Group::stop`] stops it as the gate does. A test that
    /// ends before (an assert, a panic) kills the group as it drops the
    /// guard (task 1580).
    fn group_leader(script: &str) -> Group {
        let leader = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        Group {
            leader,
            stopped: false,
        }
    }

    /// The loop of a [`group_leader`]'s script: it runs while the test
    /// process does, so that it ends by itself after a test process that
    /// ended without the drops (killed, or exiting on a timeout).
    fn while_the_test_runs() -> String {
        format!(
            "while kill -0 {} 2>/dev/null; do sleep 1; done",
            std::process::id()
        )
    }

    struct Group {
        leader: Child,
        stopped: bool,
    }

    impl Group {
        fn stop(&mut self, grace: Duration) -> Option<ExitStatus> {
            self.stopped = true;
            stop_group(&mut self.leader, grace)
        }
    }

    impl Drop for Group {
        fn drop(&mut self) {
            // Not reaped yet, so the group's id is still the leader's.
            if !self.stopped {
                signal_group(self.leader.id(), libc::SIGKILL);
                let _ = self.leader.wait();
            }
        }
    }

    /// Wait (with a limit) until `path` holds a pid.
    fn pid_in(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "no pid in {}", path.display());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_group_whose_leader_ends_on_sigterm_is_stopped_without_waiting_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let mut leader = group_leader(&format!(
            "echo $$ > '{}'; {}",
            ready.display(),
            while_the_test_runs()
        ));
        pid_in(&ready);
        let started = Instant::now();
        let status = leader.stop(STOP_GRACE);
        let took = started.elapsed();
        // The leader ended on the SIGTERM and was reaped; a zombie leader
        // read as alive would hold this for the whole grace.
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status.unwrap()),
            Some(libc::SIGTERM)
        );
        assert!(took < STOP_GRACE / 2, "stop_group took {took:?}");
    }

    #[test]
    fn a_leader_ignoring_sigterm_is_killed_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let mut leader = group_leader(&format!(
            "trap '' TERM; echo $$ > '{}'; {}",
            ready.display(),
            while_the_test_runs()
        ));
        pid_in(&ready);
        let grace = Duration::from_secs(1);
        let started = Instant::now();
        let status = leader.stop(grace);
        assert!(started.elapsed() >= grace);
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status.unwrap()),
            Some(libc::SIGKILL)
        );
    }

    #[test]
    fn what_the_group_keeps_after_its_leader_ends_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left");
        // The leader ends on SIGTERM; the child it leaves in the group
        // ignores it.
        let mut leader = group_leader(&format!(
            "sh -c 'trap \"\" TERM; echo $$ > \"{}\"; {}' & wait",
            left.display(),
            while_the_test_runs()
        ));
        let child = pid_in(&left);
        let started = Instant::now();
        leader.stop(STOP_GRACE);
        assert!(started.elapsed() < STOP_GRACE / 2);
        let deadline = Instant::now() + Duration::from_secs(20);
        while alive(child) {
            assert!(Instant::now() < deadline, "{child} outlived its group");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn settings(dir: &Path, command: &str, timeout: Duration) -> E2eSettings {
        E2eSettings {
            command: Some(command.to_owned()),
            timeout,
            run_env_root: Some(dir.join("repo")),
            queue_dir: Some(dir.join("queue")),
            scratch: dir.join("scratch"),
            log: dir.join("logs").join("e2e.log"),
            utc_offset_secs: 0,
            lock: None,
            git_env: Vec::new(),
        }
    }

    /// Which names of the starting process's environment the e2e is given:
    /// those passed by name or prefix, but never a credential.
    #[test]
    fn only_the_passed_names_and_no_credential_are_given() {
        for name in [
            "PATH",
            "HOME",
            "LC_ALL",
            "XDG_CONFIG_HOME",
            "RUSTUP_TOOLCHAIN",
            "DAGQ_E2E_WINDOWS",
        ] {
            assert!(passed(name), "{name}");
        }
        for name in [
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "AWS_SECRET_ACCESS_KEY",
            "LC_SECRET",
            "DAGQ_QUEUE",
            "DAGQ_BROKER_TOKEN_FILE",
            "ANY_NAME",
            "path",
        ] {
            assert!(!passed(name), "{name}");
        }
        assert!(credential("gh_token") && credential("SSH_AUTH_SOCK"));
        assert!(!credential("PATH"));
    }

    /// The e2e and its rerun of a `[run.env]` naming sccache are refused
    /// the server's start, through the guard beside their fixtures when
    /// the server listens, else without `RUSTC_WRAPPER`, which the outcome
    /// says (ADR-t2086-1).
    #[test]
    fn the_e2e_and_its_rerun_are_refused_the_servers_start_and_guarded_as_looked() {
        use crate::application::sccache::LookedAt;
        use crate::domain::sccache::REFUSED_ERROR_LOG;
        for (server, guarded) in [
            (LookedAt::new(Ok(true), true), true),
            (LookedAt::new(Ok(false), true), false),
            (LookedAt::new(Ok(true), false), false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            fs::create_dir_all(dir.join("repo")).unwrap();
            fs::write(
                dir.join("repo").join("dagq.toml"),
                "[run.env]\nRUSTC_WRAPPER = '/opt/bin/sccache'\nSCCACHE_SERVER_PORT = '4300'\n",
            )
            .unwrap();
            let received = dir.join("received");
            let received_arg =
                crate::application::naming::shell_quote(&received.display().to_string());
            let settings = settings(
                dir,
                &format!(
                    "echo \"${{RUSTC_WRAPPER-unset}} ${{SCCACHE_ERROR_LOG-unset}}\" >> {received_arg}; \
                     if [ -n \"${{DAGQ_E2E_RERUN:-}}\" ]; then echo 'test e2e::a ... ok'; exit 0; fi; \
                     echo 'test e2e::a ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed'; exit 101"
                ),
                Duration::from_secs(30),
            );
            let outcome =
                run_inheriting(dir, Some(&dir.join("target")), &settings, [], &server).unwrap();
            assert!(outcome.rerun.is_some(), "{outcome:?}");
            let wrapper = if guarded {
                settings.scratch.join(GUARD).display().to_string()
            } else {
                "unset".to_owned()
            };
            assert_eq!(
                fs::read_to_string(&received).unwrap(),
                format!("{wrapper} {REFUSED_ERROR_LOG}\n").repeat(2)
            );
            match &outcome.sccache_wrapper_removed {
                None => assert!(guarded),
                Some((port, why)) => {
                    assert!(!guarded);
                    assert_eq!(*port, 4300);
                    assert!(!why.is_empty());
                }
            }
        }
    }

    const GUARD: &str = crate::domain::sccache::GUARD_NAME;

    /// A failing e2e names its failed tests; one past its timeout is
    /// stopped, and what it left (a process in its `TMPDIR`) cleaned up.
    #[test]
    fn a_failing_or_stopped_e2e_is_not_passed_and_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let failing = settings(
            dir,
            "echo 'test e2e::broken ... FAILED'; exit 101",
            Duration::from_secs(30),
        );
        let outcome = run(dir, None, &failing).unwrap();
        assert!(!outcome.passed);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.failed_tests, ["e2e::broken"]);
        assert!(
            outcome
                .failure(&failing)
                .starts_with("the e2e failed: e2e::broken; see ")
        );

        let stopped = settings(
            dir,
            "cp /bin/sleep \"$TMPDIR/sleeper\"; (\"$TMPDIR/sleeper\" 60 &) ; sleep 60",
            Duration::from_secs(1),
        );
        let started = Instant::now();
        let outcome = run(dir, None, &stopped).unwrap();
        assert!(outcome.timed_out && !outcome.passed, "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(
            outcome
                .failure(&stopped)
                .contains("did not finish within 1s"),
            "{}",
            outcome.failure(&stopped)
        );
        assert_eq!(outcome.cleanup["removed"], true, "{outcome:?}");
        assert!(subdirectories(&stopped.scratch).is_empty());
    }

    /// A file that cannot be read is no marks, with why.
    #[test]
    fn the_marks_are_read_from_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        assert_eq!(read_quarantine(dir), QuarantineFile::Absent);
        fs::create_dir_all(dir.join(e2e_quarantine::FILE)).unwrap();
        assert!(matches!(
            read_quarantine(dir),
            QuarantineFile::Unreadable(_)
        ));
        fs::remove_dir(dir.join(e2e_quarantine::FILE)).unwrap();
        fs::write(dir.join(e2e_quarantine::FILE), "[x]\n").unwrap();
        assert!(matches!(
            read_quarantine(dir),
            QuarantineFile::Unreadable(error) if error.contains("[[test]]")
        ));
    }

    /// A failed first run is rerun by its failed tests only when it ended
    /// within its timeout with libtest's summary and named a failed test
    /// (ADR-t1165-1).
    #[test]
    fn only_a_finished_failing_e2e_naming_its_failures_is_rerun() {
        let failing = "test e2e::a ... FAILED\ntest e2e::b ... FAILED\ntest e2e::c ... ok\n\
test result: FAILED. 1 passed; 2 failed\n";
        assert_eq!(
            to_rerun(failing, false, false),
            Some(vec!["e2e::a".to_owned(), "e2e::b".to_owned()])
        );
        // Passed, or stopped past its timeout.
        assert_eq!(to_rerun(failing, true, false), None);
        assert_eq!(to_rerun(failing, false, true), None);
        // Naming no failed test, or cut off before libtest's summary.
        assert_eq!(
            to_rerun("test result: FAILED. 0 passed; 0 failed\n", false, false),
            None
        );
        assert_eq!(to_rerun("test e2e::a ... FAILED\n", false, false), None);
    }

    /// Only a test the rerun reports `ok` passed it: one it failed or never
    /// reported (cut off, silent) failed it, and all did when it ran past
    /// its timeout.
    #[test]
    fn only_the_tests_the_rerun_reports_ok_passed_it() {
        let rerun = ["e2e::a".to_owned(), "e2e::b".to_owned()];
        assert_eq!(
            still_failing(
                &rerun,
                "test e2e::a ... ok\ntest e2e::b ... FAILED\n",
                false
            ),
            ["e2e::b"]
        );
        assert!(
            still_failing(&rerun, "test e2e::a ... ok\ntest e2e::b ... ok\n", false).is_empty()
        );
        assert_eq!(still_failing(&rerun, "test e2e::a ... ok\n", true), rerun);
        assert_eq!(still_failing(&rerun, "", false), rerun);
        assert_eq!(
            still_failing(&rerun, "test e2e::b ... FAILED\n", false),
            rerun
        );
    }

    /// The e2e is given the passed of the starting process's environment,
    /// the `[run.env]` over them and the gate's own over both, and no
    /// credential or other name of it; the rerun is told its tests.
    #[test]
    fn the_e2e_is_given_the_passed_env_the_run_env_and_the_gates_own() {
        let inherited = passed_env(
            [
                ("PATH", "/usr/bin:/bin"),
                ("HOME", "/home/someone"),
                ("LC_ALL", "C"),
                ("DAGQ_E2E_WINDOWS", "1"),
                ("TMPDIR", "/tmp/person"),
                ("GITHUB_TOKEN", "github-token-value"),
                ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
                ("GIT_ASKPASS", "/usr/bin/askpass"),
                ("AWS_SECRET_ACCESS_KEY", "aws-value"),
                ("ANY_NAME", "any-value"),
                ("DAGQ_QUEUE", "/q/queue.db"),
            ]
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        let env = [
            ("GATE_ENV".to_owned(), "run-env-value".to_owned()),
            ("HOME".to_owned(), "/run/home".to_owned()),
            // Never the e2e's (ADR-t2589-1 decision 2).
            (
                crate::domain::local_checks::DIR_ENV.to_owned(),
                "/q/runs/r/local-checks".to_owned(),
            ),
        ];
        let given = |rerun: Option<&[String]>| -> BTreeMap<String, String> {
            given_env(
                &inherited,
                &env,
                Path::new("/scratch/1-2"),
                Some(Path::new("/target")),
                rerun,
            )
            .into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect()
        };
        let first = BTreeMap::from(
            [
                ("CARGO_TARGET_DIR", "/target"),
                ("DAGQ_E2E_WINDOWS", "1"),
                ("GATE_ENV", "run-env-value"),
                ("HOME", "/run/home"),
                ("LC_ALL", "C"),
                ("PATH", "/usr/bin:/bin"),
                ("TMPDIR", "/scratch/1-2"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
        );
        assert_eq!(given(None), first);
        let mut rerun = first;
        rerun.insert(RERUN_ENV.to_owned(), "e2e::a e2e::b".to_owned());
        assert_eq!(
            given(Some(&["e2e::a".to_owned(), "e2e::b".to_owned()])),
            rerun
        );
    }

    /// The gate runs every e2e, whatever the host lacks (ADR-t2159-1
    /// decision 7): cargo's command names no test to skip, and the rerun
    /// adds only its tests by whole name.
    #[test]
    fn the_gate_runs_every_e2e_with_nothing_left_out() {
        let cargo = |rerun: Option<&[String]>| {
            let settings = E2eSettings {
                command: None,
                ..settings(Path::new("/q"), "", Duration::from_secs(30))
            };
            e2e_command(&settings, rerun)
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            cargo(None),
            ["test", "--locked", "--test", "e2e", "--", "--ignored"]
        );
        assert_eq!(
            cargo(Some(&["e2e::a".to_owned()])),
            [
                "test",
                "--locked",
                "--test",
                "e2e",
                "--",
                "--ignored",
                "--exact",
                "e2e::a"
            ]
        );
        let shell = e2e_command(
            &settings(Path::new("/q"), "echo ok", Duration::from_secs(30)),
            None,
        );
        assert_eq!(shell.get_program(), "/bin/sh");
        assert_eq!(
            shell.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("echo ok")]
        );
    }
}
