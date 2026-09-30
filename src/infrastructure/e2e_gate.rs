//! The e2e gate on this machine (ADR-t963-1 decision 1): before a build of
//! dagq's source is put in place, `cargo test --locked --test e2e --
//! --ignored` runs in its checkout with the `[run.env]` of the repository,
//! the cmux the queue uses and a `TMPDIR` of its own under the gate's
//! scratch directory. Past its timeout its process group is stopped. After
//! it, whatever it left is cleaned up by where it lives: the processes
//! whose command line names that `TMPDIR`, the cmux workspace groups of the
//! throwaway queues its fixtures made there (their external ID is the queue
//! hash, the directory under `<fixture>/data/dagq/`), and the directory
//! itself. Groups are chosen by those hashes, never by the `[dagq-e2e]`
//! name a worker's e2e gives its groups too.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::application::broker::{self, MachineSpec, Reconnecting};
use crate::application::install::{
    E2eOutcome, E2eRerun, E2eSettings, E2eSkip, PODMAN_E2E, PodmanCheck,
};
use crate::domain::e2e_quarantine::{self, QuarantineFile};
use crate::infrastructure::broker_podman::{FileLock, PodmanCli};

/// How long one cmux call of the gate (`ping`, a group's listing or
/// deletion) and the `ps` of the cleanup may take.
const CALL_LIMIT: Duration = Duration::from_secs(20);

/// How long a stopped e2e (or a process it left) is given to end on
/// SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Run the e2e of `checkout` as the module says; an error is an e2e that
/// could not start.
pub fn run(
    checkout: &Path,
    target_dir: Option<&Path>,
    settings: &E2eSettings,
) -> Result<E2eOutcome> {
    let env = match &settings.run_env_root {
        Some(root) => super::run_env::load_run_env(
            root,
            settings.queue_dir.as_deref().unwrap_or(&settings.scratch),
            &settings.scratch,
        )
        .context("read the [run.env] the e2e runs with")?,
        None => Vec::new(),
    };
    if let Some(cmux) = &settings.cmux {
        ping(cmux)?;
    }
    let skipped = settings.podman.as_ref().and_then(|check| {
        podman_answers(check).err().map(|reason| E2eSkip {
            tests: PODMAN_E2E.iter().map(|test| (*test).to_owned()).collect(),
            reason,
        })
    });
    fs::create_dir_all(&settings.scratch)
        .with_context(|| format!("create {}", settings.scratch.display()))?;
    // What an earlier gate left when it was stopped before its cleanup; a
    // gate still running (an install next to the automatic update's job)
    // keeps its own.
    for earlier in subdirectories(&settings.scratch) {
        if !owner(&earlier).is_some_and(alive) {
            clean_up(&earlier, settings.cmux.as_deref());
        }
    }
    let first = pass(
        checkout,
        target_dir,
        settings,
        &env,
        &Pass {
            log: &settings.log,
            skipped: skipped.as_ref(),
            rerun: None,
        },
    )?;
    let passed = !first.timed_out && first.success;
    let failed = failed_tests(&first.output);
    // The failed tests once more by name, as the rerun of the landing's
    // verification (ADR-t1165-1). An e2e past its timeout, one that named
    // no failed test, or one that ended before libtest's summary (a
    // `within` past its limit exits the binary, cutting off tests that
    // then name no result) is not rerun.
    let finished = first
        .output
        .lines()
        .any(|line| line.trim_start().starts_with("test result: FAILED"));
    let rerun = (!passed && !first.timed_out && finished && !failed.is_empty()).then(|| {
        let rerun_log = settings.rerun_log();
        match pass(
            checkout,
            target_dir,
            settings,
            &env,
            &Pass {
                log: &rerun_log,
                skipped: skipped.as_ref(),
                rerun: Some(&failed),
            },
        ) {
            Ok(again) => {
                // Only a test the rerun reports `ok` passed it: one it
                // failed, or never reported (cut off, past the timeout),
                // failed it.
                let passed = passed_tests(&again.output);
                let still: Vec<String> = failed
                    .iter()
                    .filter(|test| again.timed_out || !passed.contains(test))
                    .cloned()
                    .collect();
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
        skipped,
        rerun,
        quarantine: read_quarantine(checkout),
    })
}

/// The marks of `checkout`'s `.config/e2e-quarantine.toml` (ADR-t1165-1).
pub fn read_quarantine(checkout: &Path) -> QuarantineFile {
    match fs::read_to_string(checkout.join(e2e_quarantine::FILE)) {
        Ok(text) => QuarantineFile::of(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => QuarantineFile::Absent,
        Err(error) => QuarantineFile::Unreadable(error.to_string()),
    }
}

/// One run of the e2e of the gate: where its output goes, the tests not run
/// and, for the rerun, the tests to run by name.
struct Pass<'a> {
    log: &'a Path,
    skipped: Option<&'a E2eSkip>,
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
    if let Some(skipped) = how.skipped {
        let _ = writeln!(log, "== {}", skipped.sentence());
    }
    let skip_filters: Vec<&str> = how
        .skipped
        .iter()
        .flat_map(|skipped| skipped.tests.iter().map(String::as_str))
        .collect();
    let mut command = match &settings.command {
        Some(command) => {
            let mut shell = Command::new("/bin/sh");
            shell.args(["-c", command]);
            shell
        }
        None => {
            let mut cargo =
                Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
            cargo.args(["test", "--locked", "--test", "e2e", "--", "--ignored"]);
            match how.rerun {
                // `--exact` makes every filter a whole name, the skipped
                // ones included, so they are left out: a test not run is
                // not among the failed.
                Some(tests) => {
                    cargo.arg("--exact").args(tests);
                }
                None => {
                    for filter in &skip_filters {
                        cargo.args(["--skip", filter]);
                    }
                }
            }
            cargo
        }
    };
    if !skip_filters.is_empty() {
        command.env(SKIP_ENV, skip_filters.join(" "));
    }
    if let Some(tests) = how.rerun {
        command.env(RERUN_ENV, tests.join(" "));
    }
    command
        .current_dir(checkout)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .env("TMPDIR", &root)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .process_group(0);
    if let Some(dir) = target_dir {
        command.env("CARGO_TARGET_DIR", dir);
    }
    if let Some(cmux) = &settings.cmux {
        command.env("DAGQ_E2E_CMUX", cmux);
    }
    let started = Instant::now();
    let spawned = command.spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            clean_up(&root, settings.cmux.as_deref());
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
                stop_group(child.id());
                break child.wait().ok();
            }
            Ok(None) => thread::sleep(Duration::from_millis(200)),
            Err(_) => {
                stop_group(child.id());
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
    let cleanup = clean_up(&root, settings.cmux.as_deref());
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

/// The env naming the `--skip` filters of the tests the gate does not run
/// (space separated), for a `command` in place of cargo's.
pub const SKIP_ENV: &str = "DAGQ_E2E_SKIP";

/// Whether dagq's machine is ready and its connection answers, waiting
/// for a lost connection within `check`'s bounds (ADR-t1162-1); why not.
fn podman_answers(check: &PodmanCheck) -> std::result::Result<(), String> {
    let podman =
        PodmanCli::resolve(check.executable.as_deref()).map_err(|error| error.to_string())?;
    let podman = Reconnecting {
        inner: podman,
        reconnect: check.reconnect,
    };
    let lock = FileLock::machine(&check.lock_home);
    broker::connect(&podman, &lock, &MachineSpec::default())
        .map(|_| ())
        .map_err(|error| error.to_string())
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

/// `cmux ping`, or why the e2e cannot start.
fn ping(cmux: &Path) -> Result<()> {
    let output = bounded(Command::new(cmux).arg("ping")).with_context(|| {
        format!(
            "the e2e needs a running cmux, and {} did not run",
            cmux.display()
        )
    })?;
    ensure!(
        output.status.success(),
        "the e2e needs a running cmux, and `{} ping` failed ({}): {}",
        cmux.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Run `command` to its end within [`CALL_LIMIT`], its output captured;
/// past it the command is killed and it is an error.
fn bounded(command: &mut Command) -> Result<Output> {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let (sent, received) = mpsc::channel();
    thread::spawn(move || {
        let _ = sent.send(child.wait_with_output());
    });
    match received.recv_timeout(CALL_LIMIT) {
        Ok(output) => Ok(output?),
        Err(_) => {
            signal(pid, libc::SIGKILL);
            bail!("it did not end within {}s", CALL_LIMIT.as_secs())
        }
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

/// Stop the process group `leader` leads: SIGTERM, then SIGKILL after
/// [`STOP_GRACE`].
fn stop_group(leader: u32) {
    signal_group(leader, libc::SIGTERM);
    let deadline = Instant::now() + STOP_GRACE;
    while alive(leader) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    signal_group(leader, libc::SIGKILL);
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

/// The queue hashes of the throwaway queues the e2e's fixtures made under
/// `root`: the directories under `<fixture>/data/dagq/`.
pub fn queue_hashes(root: &Path) -> Vec<String> {
    let mut hashes = Vec::new();
    for fixture in subdirectories(root) {
        for queue in subdirectories(&fixture.join("data").join("dagq")) {
            if let Some(name) = queue.file_name() {
                hashes.push(name.to_string_lossy().into_owned());
            }
        }
    }
    hashes.sort();
    hashes
}

/// Clean up what the e2e left under `root`: its processes, its groups in
/// `cmux` and the directory, which stays when a group could not be deleted
/// (the next gate finds it by the same hashes). What was done.
pub fn clean_up(root: &Path, cmux: Option<&Path>) -> Value {
    let stopped = stop_processes_inside(root);
    let hashes = queue_hashes(root);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    if let (Some(cmux), false) = (cmux, hashes.is_empty()) {
        match delete_groups(cmux, &hashes) {
            Ok(groups) => deleted = groups,
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    let removed = errors.is_empty()
        && match fs::remove_dir_all(root) {
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
        "groups": deleted,
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

/// Delete the workspace groups whose external ID is one of `hashes`, and
/// close what is left in them; the IDs of the groups deleted.
fn delete_groups(cmux: &Path, hashes: &[String]) -> Result<Vec<String>> {
    let output = bounded(Command::new(cmux).args([
        "--json",
        "--id-format",
        "uuids",
        "workspace-group",
        "list",
    ]))
    .context("list cmux's workspace groups")?;
    ensure!(
        output.status.success(),
        "cmux workspace-group list failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let list: Value =
        serde_json::from_slice(&output.stdout).context("decode cmux workspace-group list")?;
    let groups = list["groups"]
        .as_array()
        .with_context(|| format!("cmux workspace-group list has no groups: {list}"))?;
    let mut deleted = Vec::new();
    for group in groups.iter().filter(|group| {
        group["external_id"]
            .as_str()
            .is_some_and(|id| hashes.iter().any(|hash| hash == id))
    }) {
        let id = group["id"].as_str().unwrap_or_default();
        let output = bounded(Command::new(cmux).args([
            "workspace-group",
            "delete",
            id,
            "--close-workspaces",
        ]))
        .with_context(|| format!("delete workspace group {id}"))?;
        ensure!(
            output.status.success(),
            "deleting workspace group {id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        deleted.push(id.to_owned());
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_failed_tests_are_read_from_the_output_once_each() {
        let output = "running 3 tests\ntest a::one ... ok\ntest a::two ... FAILED\n\
test b ... FAILED\nfailures:\n    a::two\ntest a::two ... FAILED\n";
        assert_eq!(failed_tests(output), ["a::two", "b"]);
        assert!(failed_tests("test a ... ok\n").is_empty());
    }

    /// A fake cmux that answers `ping`, lists one group of the queue hash
    /// `q1` and one of another queue, and records what it was asked.
    fn fake_cmux(dir: &Path, ping_fails: bool) -> PathBuf {
        let cmux = dir.join("cmux");
        let calls = dir.join("cmux-calls");
        fs::write(
            &cmux,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n  ping) {} ;;\n  \
*'workspace-group list'*) echo '{{\"groups\":[{{\"id\":\"G-1\",\"external_id\":\"q1\"}},\
{{\"id\":\"G-2\",\"external_id\":\"production\"}}]}}' ;;\nesac\n",
                calls.display(),
                if ping_fails {
                    "echo no socket >&2; exit 1"
                } else {
                    "echo PONG"
                }
            ),
        )
        .unwrap();
        fs::set_permissions(&cmux, fs::Permissions::from_mode(0o755)).unwrap();
        cmux
    }

    fn settings(dir: &Path, command: &str, timeout: Duration) -> E2eSettings {
        E2eSettings {
            command: Some(command.to_owned()),
            timeout,
            cmux: Some(fake_cmux(dir, false)),
            run_env_root: Some(dir.join("repo")),
            queue_dir: Some(dir.join("queue")),
            scratch: dir.join("scratch"),
            log: dir.join("logs").join("e2e.log"),
            podman: None,
            utc_offset_secs: 0,
        }
    }

    /// A fake podman: dagq's machine runs, and its connection answers
    /// `info` unless `cut`, when every command on it loses the connection.
    fn fake_podman(dir: &Path, cut: bool) -> PodmanCheck {
        let podman = dir.join(if cut { "podman-cut" } else { "podman" });
        fs::write(
            &podman,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  'machine list'*) \
echo '[{{\"Name\":\"dagq\",\"Running\":true}}]' ;;\n  '--connection dagq info'*) {} ;;\nesac\n",
                if cut {
                    "echo 'Error: unable to connect to Podman socket: failed to connect: ssh: \
handshake failed: read tcp 127.0.0.1:1->127.0.0.1:65003: read: connection reset by peer' >&2; \
exit 125"
                } else {
                    "echo 6.1.2"
                }
            ),
        )
        .unwrap();
        fs::set_permissions(&podman, fs::Permissions::from_mode(0o755)).unwrap();
        PodmanCheck {
            executable: Some(podman),
            lock_home: dir.join("config"),
            reconnect: crate::application::broker::Reconnect {
                reruns: 1,
                probes: 2,
                interval: Duration::ZERO,
            },
        }
    }

    /// Podman that answers runs every e2e; podman that cannot be reached
    /// has the podman tests skipped (`--skip` through `DAGQ_E2E_SKIP`),
    /// named with the reason in the outcome, its report and the log
    /// (ADR-t1162-1); podman that is missing is the same.
    #[test]
    fn podman_that_cannot_be_reached_skips_only_the_podman_e2e_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let command = "echo \"skip [$DAGQ_E2E_SKIP]\"; echo 'test e2e::a ... ok'";
        let mut settings = settings(dir, command, Duration::from_secs(30));

        settings.podman = Some(fake_podman(dir, false));
        let outcome = run(dir, None, &settings).unwrap();
        assert!(outcome.passed && outcome.skipped.is_none(), "{outcome:?}");
        assert!(
            fs::read_to_string(&settings.log)
                .unwrap()
                .contains("skip []")
        );
        assert!(outcome.report(&settings).get("skipped").is_none());
        assert!(dir.join("config/dagq/podman-machine.lock").exists());

        settings.podman = Some(fake_podman(dir, true));
        let outcome = run(dir, None, &settings).unwrap();
        assert!(outcome.passed, "{outcome:?}");
        let skipped = outcome.skipped.clone().unwrap();
        assert_eq!(skipped.tests, ["broker::"]);
        assert!(skipped.reason.contains("handshake failed"), "{skipped:?}");
        assert!(skipped.reason.contains("did not answer"), "{skipped:?}");
        let log = fs::read_to_string(&settings.log).unwrap();
        assert!(log.contains("skip [broker::]"), "{log}");
        assert!(
            log.contains("the e2e did not run broker:: because podman could not be reached"),
            "{log}"
        );
        let report = outcome.report(&settings);
        assert_eq!(report["status"], "passed");
        assert_eq!(report["skipped"]["tests"], json!(["broker::"]));

        settings.podman = Some(PodmanCheck {
            executable: Some(dir.join("no-podman")),
            ..fake_podman(dir, false)
        });
        let outcome = run(dir, None, &settings).unwrap();
        let skipped = outcome.skipped.unwrap();
        assert!(skipped.reason.contains("podman_missing"), "{skipped:?}");

        // A failing e2e still fails with the podman tests skipped.
        let mut failing = self::settings(
            dir,
            "echo 'test e2e::broken ... FAILED'; exit 101",
            Duration::from_secs(30),
        );
        failing.podman = Some(fake_podman(dir, true));
        let outcome = run(dir, None, &failing).unwrap();
        assert!(!outcome.passed && outcome.skipped.is_some(), "{outcome:?}");
    }

    /// The e2e runs with the `[run.env]`, the cmux and a `TMPDIR` of its
    /// own; afterwards the group of the throwaway queue it made is deleted
    /// (and no other), and its directory removed.
    #[test]
    fn a_passing_e2e_runs_with_the_env_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::write(
            dir.join("repo").join("dagq.toml"),
            "[run.env]\nGATE_ENV = '${DAGQ_QUEUE_DIR}/x'\n",
        )
        .unwrap();
        let settings = settings(
            dir,
            "mkdir -p \"$TMPDIR/.tmpA/data/dagq/q1\" && echo \"env $GATE_ENV $DAGQ_E2E_CMUX\" \
&& echo \"target $CARGO_TARGET_DIR\" && echo 'test e2e::a ... ok'",
            Duration::from_secs(30),
        );
        let outcome = run(dir, Some(&dir.join("target")), &settings).unwrap();
        assert!(outcome.passed, "{outcome:?}");
        assert!(!outcome.timed_out);
        assert!(outcome.failed_tests.is_empty());
        let log = fs::read_to_string(&settings.log).unwrap();
        assert!(
            log.contains(&format!("env {}/x", dir.join("queue").display())),
            "{log}"
        );
        assert!(log.contains(&format!("target {}", dir.join("target").display())));
        assert_eq!(outcome.cleanup["groups"], json!(["G-1"]), "{outcome:?}");
        assert_eq!(outcome.cleanup["removed"], true);
        assert!(subdirectories(&settings.scratch).is_empty());
        let calls = fs::read_to_string(dir.join("cmux-calls")).unwrap();
        assert!(calls.contains("workspace-group delete G-1 --close-workspaces"));
        assert!(!calls.contains("G-2"), "{calls}");
    }

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

    /// A failing e2e reruns its failed tests by name (`DAGQ_E2E_RERUN` for
    /// a command) in a `TMPDIR` of its own, writing to the rerun's log and
    /// cleaning up after it; the tests that failed again are named, and the
    /// checkout's marks are read (ADR-t1165-1). Only a test the rerun
    /// reports `ok` passed it: one past its timeout, or that reports none or
    /// not all, fails the rest. An e2e past its timeout, naming no failed
    /// test or cut off before libtest's summary is not rerun.
    #[test]
    fn a_failing_e2e_reruns_its_failed_tests_by_name_once() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let command = "if [ -n \"$DAGQ_E2E_RERUN\" ]; then echo \"rerun [$DAGQ_E2E_RERUN] $TMPDIR\"; \
mkdir -p \"$TMPDIR/.tmpA/data/dagq/q1\"; echo 'test e2e::a ... ok'; echo 'test e2e::b ... FAILED'; \
exit 101; else echo 'test e2e::a ... FAILED'; echo 'test e2e::b ... FAILED'; echo 'test e2e::c ... ok'; \
echo 'test result: FAILED. 1 passed; 2 failed'; exit 101; fi";
        let settings = settings(dir, command, Duration::from_secs(30));
        fs::create_dir_all(dir.join(".config")).unwrap();
        fs::write(
            dir.join(e2e_quarantine::FILE),
            "[[test]]\nname = \"e2e::b\"\nreason = \"flaky\"\ntask = 1120\nuntil = 2026-10-15\n",
        )
        .unwrap();
        let outcome = run(dir, None, &settings).unwrap();
        assert!(!outcome.passed, "{outcome:?}");
        assert_eq!(outcome.failed_tests, ["e2e::a", "e2e::b"]);
        let rerun = outcome.rerun.clone().unwrap();
        assert_eq!(rerun.tests, ["e2e::a", "e2e::b"]);
        assert_eq!(rerun.failed, ["e2e::b"]);
        assert!(!rerun.timed_out && rerun.error.is_none(), "{rerun:?}");
        assert_eq!(rerun.cleanup["groups"], json!(["G-1"]), "{rerun:?}");
        assert_eq!(rerun.cleanup["removed"], true);
        assert!(subdirectories(&settings.scratch).is_empty());
        let QuarantineFile::Marks(marks) = &outcome.quarantine else {
            panic!("{:?}", outcome.quarantine);
        };
        assert_eq!(marks[0].name, "e2e::b");
        let first = fs::read_to_string(&settings.log).unwrap();
        assert!(!first.contains("rerun ["), "{first}");
        let again = fs::read_to_string(settings.rerun_log()).unwrap();
        assert!(again.contains("rerun [e2e::a e2e::b]"), "{again}");
        assert!(
            again.contains("== the rerun by name of the e2e tests that failed in")
                && again.contains(".rerun-"),
            "{again}"
        );

        // A passing e2e is not rerun, and no file means no marks.
        let passing = self::settings(dir, "echo 'test e2e::a ... ok'", Duration::from_secs(30));
        fs::remove_file(dir.join(e2e_quarantine::FILE)).unwrap();
        let outcome = run(dir, None, &passing).unwrap();
        assert!(outcome.passed && outcome.rerun.is_none(), "{outcome:?}");
        assert_eq!(outcome.quarantine, QuarantineFile::Absent);

        // A rerun past its timeout, naming nothing, or cut off before a
        // test reports, fails what it did not report `ok`.
        let failing = "echo 'test e2e::a ... FAILED'; echo 'test e2e::b ... FAILED'; \
echo 'test result: FAILED. 0 passed; 2 failed'; exit 101";
        let stuck = self::settings(
            dir,
            &format!(
                "if [ -n \"$DAGQ_E2E_RERUN\" ]; then echo 'test e2e::a ... ok'; sleep 30; fi; {failing}"
            ),
            Duration::from_secs(1),
        );
        let rerun = run(dir, None, &stuck).unwrap().rerun.unwrap();
        assert!(rerun.timed_out, "{rerun:?}");
        assert_eq!(rerun.failed, ["e2e::a", "e2e::b"]);
        let silent = self::settings(
            dir,
            &format!("if [ -n \"$DAGQ_E2E_RERUN\" ]; then exit 1; fi; {failing}"),
            Duration::from_secs(30),
        );
        let rerun = run(dir, None, &silent).unwrap().rerun.unwrap();
        assert_eq!(rerun.failed, ["e2e::a", "e2e::b"], "{rerun:?}");
        let cut_off = self::settings(
            dir,
            &format!(
                "if [ -n \"$DAGQ_E2E_RERUN\" ]; then echo 'test e2e::b ... FAILED'; exit 101; fi; {failing}"
            ),
            Duration::from_secs(30),
        );
        let rerun = run(dir, None, &cut_off).unwrap().rerun.unwrap();
        assert_eq!(rerun.failed, ["e2e::a", "e2e::b"], "{rerun:?}");

        // An e2e past its timeout, naming no failed test, or cut off before
        // libtest's summary (a test left without a result) is not rerun.
        let unnamed = self::settings(dir, "exit 101", Duration::from_secs(30));
        assert!(run(dir, None, &unnamed).unwrap().rerun.is_none());
        let exited = self::settings(
            dir,
            "echo 'test e2e::a ... FAILED'; exit 101",
            Duration::from_secs(30),
        );
        let outcome = run(dir, None, &exited).unwrap();
        assert_eq!(outcome.failed_tests, ["e2e::a"]);
        assert!(outcome.rerun.is_none(), "{outcome:?}");
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

    /// An earlier gate's directory is cleaned up before the e2e unless its
    /// gate still runs.
    #[test]
    fn a_gate_cleans_up_what_a_gone_gate_left_but_not_a_running_one() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let settings = settings(dir, "exit 0", Duration::from_secs(30));
        // No pid is this large; this test's own is alive.
        let gone = settings.scratch.join("1-4000000000");
        let running = settings.scratch.join(format!("1-{}", std::process::id()));
        fs::create_dir_all(gone.join(".tmpA/data/dagq/q1")).unwrap();
        fs::create_dir_all(&running).unwrap();
        assert!(run(dir, None, &settings).unwrap().passed);
        assert!(!gone.exists());
        assert!(running.exists());
        let calls = fs::read_to_string(dir.join("cmux-calls")).unwrap();
        assert!(calls.contains("workspace-group delete G-1"), "{calls}");
    }

    /// Without a running cmux the e2e does not start.
    #[test]
    fn the_e2e_needs_a_running_cmux() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let mut settings = settings(dir, "exit 0", Duration::from_secs(5));
        settings.cmux = Some(fake_cmux(dir, true));
        let error = run(dir, None, &settings).unwrap_err();
        assert!(
            format!("{error:#}").contains("needs a running cmux"),
            "{error:#}"
        );
        assert!(!settings.log.exists());
    }
}
