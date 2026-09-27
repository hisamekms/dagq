//! What the e2e tests open in cmux and on disk, and how it is closed again:
//! the guards that close a test's workspaces and group when it ends, the
//! listing of every cmux window's workspaces they look at, and the sweep that
//! cleans up after earlier e2e processes that died before their guards ran.
use super::{CLEANUP_LIMIT, pid_alive};
use crate::common::{self, Bounded, Cleanup};
use anyhow::{Context, bail};
use dagq::infrastructure::adapters::merged_workspace_listing;
use serde_json::Value;
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// How many times [`cmux_retrying`] runs a cmux query, and how long it waits
/// between tries. cmux can answer `ping`, `workspace list` or
/// `workspace-group list` with a failure for a moment while other e2e tests
/// running in parallel open and close workspaces; a few short retries ride that out, where running the e2e
/// tests one at a time would lengthen the whole run.
const CMUX_RETRY_ATTEMPTS: u32 = 5;
const CMUX_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Run `cmux args` until `accept` holds of its output, up to
/// [`CMUX_RETRY_ATTEMPTS`] times [`CMUX_RETRY_INTERVAL`] apart, and return
/// the last output. A cmux that cannot be started at all is returned at once:
/// that is not a transient failure.
pub(crate) fn cmux_retrying(
    cmux: &Path,
    args: &[&str],
    accept: impl Fn(&std::process::Output) -> bool,
) -> std::io::Result<std::process::Output> {
    let mut attempt = 1;
    loop {
        let output = Command::new(cmux).args(args).bounded_output()?;
        if accept(&output) || attempt >= CMUX_RETRY_ATTEMPTS {
            return Ok(output);
        }
        eprintln!(
            "cmux {args:?} failed ({}), retrying ({attempt}/{CMUX_RETRY_ATTEMPTS}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        attempt += 1;
        thread::sleep(CMUX_RETRY_INTERVAL);
    }
}

/// The JSON reply of `cmux args`, retried like [`cmux_retrying`].
fn cmux_query(cmux: &Path, args: &[&str]) -> anyhow::Result<Value> {
    let output = cmux_retrying(cmux, args, |output| {
        output.status.success() && serde_json::from_slice::<Value>(&output.stdout).is_ok()
    })
    .with_context(|| format!("run {} {args:?}", cmux.display()))?;
    if !output.status.success() {
        bail!(
            "cmux {args:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).with_context(|| format!("decode cmux {args:?}"))
}

/// The workspaces of every cmux window, the way the runtime lists them
/// (`list-windows`, then `workspace list --window` for each): without
/// `--window` cmux lists only the caller's window, and a workspace a person
/// moved to another window would look closed. A window that cannot be listed
/// fails the listing; since a window can close between `list-windows` and
/// its `workspace list` (another e2e test closing the window it opened),
/// the whole listing is retried like [`cmux_retrying`] before it fails.
pub(crate) fn all_workspaces(cmux: &Path) -> anyhow::Result<Vec<Value>> {
    let mut attempt = 1;
    loop {
        match merged_listing(cmux) {
            Ok(listing) => {
                return Ok(listing["workspaces"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default());
            }
            Err(error) if attempt >= CMUX_RETRY_ATTEMPTS => return Err(error),
            Err(error) => eprintln!(
                "listing every window's workspaces failed, retrying \
                 ({attempt}/{CMUX_RETRY_ATTEMPTS}): {error:#}"
            ),
        }
        attempt += 1;
        thread::sleep(CMUX_RETRY_INTERVAL);
    }
}

fn merged_listing(cmux: &Path) -> anyhow::Result<Value> {
    let windows = cmux_query(cmux, &["--json", "--id-format", "uuids", "list-windows"])?;
    merged_workspace_listing(&windows, |window| {
        let args = [
            "--json",
            "--id-format",
            "uuids",
            "workspace",
            "list",
            "--window",
            window,
        ];
        let output = Command::new(cmux).args(args).bounded_output()?;
        if !output.status.success() {
            bail!(
                "cmux {args:?} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).with_context(|| format!("decode cmux {args:?}"))
    })
}

fn find_workspace(workspaces: &[Value], id: &str) -> Option<Value> {
    workspaces
        .iter()
        .find(|w| w["id"].as_str().is_some_and(|w| w.eq_ignore_ascii_case(id)))
        .cloned()
}

/// The workspace's entry in the listing of every window, while it is listed.
pub(crate) fn listed_workspace(cmux: &Path, id: &str) -> Option<Value> {
    let workspaces = all_workspaces(cmux).unwrap_or_else(|error| panic!("{error:#}"));
    find_workspace(&workspaces, id)
}

pub(crate) fn workspace_listed(cmux: &Path, id: &str) -> bool {
    listed_workspace(cmux, id).is_some()
}

/// cmux confirms a `workspace close` before the workspace leaves its
/// listing, so "gone" is waited for rather than asserted on the first look.
pub(crate) fn wait_until_not_listed(cmux: &Path, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while workspace_listed(cmux, id) {
        assert!(
            Instant::now() < deadline,
            "workspace {id} is still listed 30s after it was closed"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Unpin and close a workspace, reporting the outcome on stderr under
/// `who`. cmux refuses to close a pinned workspace (the inbox is pinned,
/// ADR-0031), and a person may pin any other.
fn close_workspace(cmux: &Path, id: &str, who: &str) {
    let _ = Command::new(cmux)
        .args(["workspace-action", "--action", "unpin", "--workspace", id])
        .bounded_output();
    match Command::new(cmux)
        .args(["workspace", "close", id])
        .bounded_output()
    {
        Ok(output) if output.status.success() => eprintln!("{who}closed workspace {id}"),
        Ok(output) => eprintln!(
            "{who}closing workspace {id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => eprintln!("{who}closing workspace {id} failed: {error}"),
    }
}

/// Closes the workspaces a test opened if they are still open when it ends,
/// on success and on panic alike. On the happy path the supervisor has
/// already closed them; cmux 0.64 also closes a workspace by itself once its
/// command exits. Either way "not listed" is the expected state, not a
/// failure. A test records a workspace as soon as a command's output names
/// it, before asserting anything about that output (see [`Self::record_opened`]).
pub(crate) struct WorkspaceGuard {
    pub(crate) cmux: PathBuf,
    pub(crate) ids: Vec<String>,
}

impl WorkspaceGuard {
    pub(crate) fn record(&mut self, id: &str) {
        if !self.ids.iter().any(|known| known.eq_ignore_ascii_case(id)) {
            self.ids.push(id.to_owned());
        }
    }

    /// Record every workspace the output of `up`, `plan` or another command
    /// that opens workspaces names: each string under a `workspace_id` key of
    /// its JSON, at any depth. When the output is not JSON (the command died
    /// halfway), every UUID in it is recorded instead: a UUID that names no
    /// workspace is just "already closed" when the guard drops.
    pub(crate) fn record_opened(&mut self, stdout: &[u8]) {
        fn walk(value: &Value, found: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    for (key, value) in map {
                        if key.ends_with("workspace_id")
                            && let Some(id) = value.as_str()
                        {
                            found.push(id.to_owned());
                        }
                        walk(value, found);
                    }
                }
                Value::Array(values) => values.iter().for_each(|value| walk(value, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        match serde_json::from_slice::<Value>(stdout) {
            Ok(value) => walk(&value, &mut found),
            Err(_) => found.extend(uuids_in(&String::from_utf8_lossy(stdout))),
        }
        for id in found {
            self.record(&id);
        }
    }
}

/// Every hyphenated UUID in `text`, wherever it starts: also one glued to
/// other letters, like `workspace-<uuid>`.
fn uuids_in(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut start = 0;
    while start + 36 <= bytes.len() {
        if let Ok(candidate) = std::str::from_utf8(&bytes[start..start + 36])
            && candidate.as_bytes()[8] == b'-'
            && uuid::Uuid::parse_str(candidate).is_ok()
        {
            found.push(candidate.to_owned());
            start += 36;
        } else {
            start += 1;
        }
    }
    found
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        // Never panic here: a panic while the test is already unwinding
        // aborts the whole test binary. Without a listing every recorded
        // workspace is closed; closing one that is gone only fails.
        let listed = all_workspaces(&self.cmux)
            .inspect_err(|error| eprintln!("listing workspaces failed: {error:#}"))
            .ok();
        for id in &self.ids {
            if let Some(workspaces) = &listed
                && find_workspace(workspaces, id).is_none()
            {
                eprintln!("workspace {id} already closed");
                continue;
            }
            close_workspace(&self.cmux, id, "");
        }
    }
}

/// The queue's workspace group in `cmux --json workspace-group list`,
/// found by its external ID (the queue hash).
pub(crate) fn listed_group(cmux: &Path, external_id: &str) -> Option<Value> {
    let list = cmux_query(
        cmux,
        &["--json", "--id-format", "uuids", "workspace-group", "list"],
    )
    .unwrap_or_else(|error| panic!("{error:#}"));
    list["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["external_id"] == external_id)
        .cloned()
}

/// Deletes the queue's workspace group and closes what is left in it (the
/// anchor cmux generated with it) when the test ends, and when a wait times
/// out and the test binary exits without unwinding (task 440).
pub(crate) struct GroupGuard {
    pub(crate) cmux: PathBuf,
    pub(crate) external_id: String,
    _on_timeout: Cleanup,
}

impl GroupGuard {
    pub(crate) fn new(cmux: PathBuf, external_id: String) -> Self {
        let on_timeout = {
            let (cmux, external_id) = (cmux.clone(), external_id.clone());
            common::on_timeout(
                CLEANUP_LIMIT,
                format!("delete the workspace group of queue {external_id}"),
                move || delete_group(&cmux, &external_id, ""),
            )
        };
        Self {
            cmux,
            external_id,
            _on_timeout: on_timeout,
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        // Never panic here: a panic while the test is already unwinding
        // aborts the whole test binary. A group left behind is swept by the
        // next fixture.
        delete_group(&self.cmux, &self.external_id, "");
    }
}

/// The file in a fixture's temporary directory that its test process holds an
/// exclusive `flock` on for as long as the fixture lives. The lock goes away
/// with the process however it ends, SIGKILL included, so a directory whose
/// owner file can be locked by someone else belongs to a dead test.
const OWNER_FILE: &str = "e2e-owner";
/// A fixture directory without an owner file, left by an e2e from before the
/// owner file existed, is swept only once it is this old.
const UNMARKED_SWEEP_AGE: Duration = Duration::from_secs(60 * 60);

/// Take ownership of a fresh fixture directory: lock the owner file under a
/// temporary name and rename it into place, so a concurrent sweep never sees
/// an owner file that is not yet locked.
pub(crate) fn claim_fixture_dir(dir: &Path) -> fs::File {
    let staging = dir.join(format!("{OWNER_FILE}.tmp"));
    let mut file = fs::File::create(&staging).unwrap();
    file.lock().unwrap();
    use std::io::Write;
    writeln!(file, "{}", std::process::id()).unwrap();
    fs::rename(&staging, dir.join(OWNER_FILE)).unwrap();
    file
}

/// Clean up what earlier e2e tests left behind when their process died before
/// its guards and `TempDir` could drop (SIGTERM, SIGKILL), or when a
/// workspace escaped its guard:
///
/// - For a fixture directory that is still there but abandoned: the
///   processes running from or on it, the workspace groups named after its
///   queue hashes, the cmux workspaces whose `DAGQ_QUEUE` or `E2E_SHARED`
///   points into it, and the directory itself. A directory counts as
///   abandoned only when its owner lock is free (the owner process is gone),
///   or, without an owner file, when it has the fixture's shape and is older
///   than [`UNMARKED_SWEEP_AGE`]. The sweep holds that lock while it works,
///   so concurrent sweeps never take the same directory.
/// - For a directory that is already gone: the workspaces whose `DAGQ_QUEUE`
///   or `E2E_SHARED` points into a `$TMPDIR/.tmp*` directory that no longer
///   exists (pinned ones included), and the group of the queue their
///   `DAGQ_QUEUE` names. Nothing else can find them once the directory is
///   removed.
///
/// Workspaces are looked for in every cmux window. Nothing else is touched:
/// the production queue's workspaces (the inbox, the supervisor, planners,
/// runs) do carry `DAGQ_QUEUE`, but it is under the data home, not
/// `$TMPDIR` (see `the_sweep_leaves_live_and_production_queues_alone`), a
/// live fixture's directory exists, and the production group has another
/// external ID.
pub(crate) fn sweep_abandoned_fixtures(cmux: &Path) {
    let temp_roots = temp_roots();
    let mut abandoned = Vec::new();
    if let Ok(entries) = fs::read_dir(env::temp_dir()) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !entry.file_name().to_string_lossy().starts_with(".tmp") {
                continue;
            }
            if let Some(lock) = claim_abandoned(&dir) {
                abandoned.push((dir, lock));
            }
        }
    }
    let prefixes: Vec<PathBuf> = abandoned
        .iter()
        .flat_map(|(dir, _)| {
            let mut forms = vec![dir.clone()];
            if let Ok(real) = dir.canonicalize()
                && real != *dir
            {
                forms.push(real);
            }
            forms
        })
        .collect();
    let inside = |value: &str| {
        prefixes
            .iter()
            .any(|form| Path::new(value).starts_with(form))
    };
    for (dir, _) in &abandoned {
        eprintln!("e2e sweep: {} was left by a dead e2e", dir.display());
    }
    if !abandoned.is_empty() {
        kill_processes_inside(&inside);
    }
    for (dir, _) in &abandoned {
        for hash in queue_hashes(dir) {
            delete_group(cmux, &hash, "e2e sweep: ");
        }
    }
    let left_behind = |value: &str| inside(value) || in_vanished_temp_dir(value, &temp_roots);
    for hash in close_workspaces_left_behind(cmux, &left_behind) {
        delete_group(cmux, &hash, "e2e sweep: ");
    }
    for (dir, lock) in abandoned {
        match fs::remove_dir_all(&dir) {
            Ok(()) => eprintln!("e2e sweep: removed {}", dir.display()),
            Err(error) => eprintln!("e2e sweep: removing {} failed: {error}", dir.display()),
        }
        drop(lock);
    }
}

/// `$TMPDIR` as the tests see it and as its symlinks resolve (on macOS
/// `/var/folders/…` is `/private/var/folders/…`, and the queue paths in a
/// workspace's env are canonical).
fn temp_roots() -> Vec<PathBuf> {
    let root = env::temp_dir();
    let mut roots = vec![root.clone()];
    if let Ok(real) = root.canonicalize()
        && real != root
    {
        roots.push(real);
    }
    roots
}

/// Whether `value` is a path inside a `.tmp*` directory directly under one of
/// `temp_roots` that no longer exists: what an e2e fixture's queue or shared
/// path looks like once its `TempDir` was removed.
fn in_vanished_temp_dir(value: &str, temp_roots: &[PathBuf]) -> bool {
    let path = Path::new(value);
    temp_roots.iter().any(|root| {
        let Ok(rest) = path.strip_prefix(root) else {
            return false;
        };
        let Some(Component::Normal(first)) = rest.components().next() else {
            return false;
        };
        first.to_string_lossy().starts_with(".tmp") && !root.join(first).exists()
    })
}

/// The locked owner file of `dir` when `dir` is a fixture directory whose
/// owner is gone, `None` when it is live, not a fixture, or taken by another
/// sweep.
fn claim_abandoned(dir: &Path) -> Option<fs::File> {
    let owner = dir.join(OWNER_FILE);
    let file = if owner.exists() {
        fs::File::open(&owner).ok()?
    } else {
        // An e2e from before the owner file: recognize the fixture by its
        // stub agent and queue data home, and wait until it is old.
        if !dir.join("claude-stub").is_file() || !dir.join("data").is_dir() {
            return None;
        }
        let age = fs::metadata(dir).ok()?.modified().ok()?.elapsed().ok()?;
        if age < UNMARKED_SWEEP_AGE {
            return None;
        }
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&owner)
            .ok()?
    };
    file.try_lock().ok()?;
    Some(file)
}

/// The queue hashes under the fixture's data home, which are the external
/// IDs of the queues' workspace groups.
fn queue_hashes(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir.join("data").join("dagq")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Terminate the processes an abandoned directory started: those whose
/// program is in it (the runner copy), the stub agent (`/bin/sh
/// <dir>/claude-stub`), and a `dagq` binary given its queue or stub
/// (`--db`, `--claude`), like the temporary queue's supervisor. `ps` joins
/// argv with spaces, so only these shapes are matched: a word of some other
/// process's arguments, like a Claude prompt that quotes such a path, never
/// makes it a target.
fn kill_processes_inside(inside: &dyn Fn(&str) -> bool) {
    let Ok(output) = Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .bounded_output()
    else {
        eprintln!("e2e sweep: ps failed; left processes alone");
        return;
    };
    let me = std::process::id();
    let mut victims = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut words = line.split_whitespace();
        let Some(pid) = words.next().and_then(|pid| pid.parse::<u32>().ok()) else {
            continue;
        };
        let argv: Vec<&str> = words.collect();
        let program = argv.first().is_some_and(|word| inside(word));
        let stub = argv.first() == Some(&"/bin/sh")
            && argv
                .get(1)
                .is_some_and(|word| word.ends_with("/claude-stub") && inside(word));
        let dagq = argv
            .first()
            .is_some_and(|word| Path::new(word).file_name() == Some("dagq".as_ref()))
            && argv
                .windows(2)
                .any(|pair| matches!(pair[0], "--db" | "--claude") && inside(pair[1]));
        if pid != me && (program || stub || dagq) {
            eprintln!("e2e sweep: terminating process {pid}: {}", argv.join(" "));
            victims.push(pid);
        }
    }
    for pid in &victims {
        // SAFETY: kill has no memory preconditions.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while victims.iter().any(|pid| pid_alive(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    for pid in victims.iter().filter(|pid| pid_alive(**pid)) {
        eprintln!("e2e sweep: killing process {pid}, still alive after SIGTERM");
        // SAFETY: as above.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Delete the workspace group of queue `external_id` with what is left in
/// it, if cmux lists it, reporting the outcome on stderr under `who`.
fn delete_group(cmux: &Path, external_id: &str, who: &str) {
    let Some(group) = try_listed_group(cmux, external_id) else {
        return;
    };
    let id = group["id"].as_str().unwrap_or_default().to_owned();
    let name = group["name"].as_str().unwrap_or_default().to_owned();
    match Command::new(cmux)
        .args(["workspace-group", "delete", &id, "--close-workspaces"])
        .bounded_output()
    {
        Ok(output) if output.status.success() => {
            eprintln!("{who}deleted workspace group {id} {name} (queue {external_id})")
        }
        Ok(output) => eprintln!(
            "{who}deleting workspace group {id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => eprintln!("{who}deleting workspace group {id} failed: {error}"),
    }
}

/// Unpin and close each workspace of every window whose `DAGQ_QUEUE` or
/// `E2E_SHARED` is `left_behind`, and return the queue hashes (the group
/// external IDs) their `DAGQ_QUEUE` names. A workspace without those
/// variables is skipped, and so is one whose variables point anywhere else,
/// like every workspace of the production queue (its `DAGQ_QUEUE` is under
/// the data home). When a window cannot be listed nothing is closed.
fn close_workspaces_left_behind(cmux: &Path, left_behind: &dyn Fn(&str) -> bool) -> Vec<String> {
    let workspaces = match all_workspaces(cmux) {
        Ok(workspaces) => workspaces,
        Err(error) => {
            eprintln!("e2e sweep: listing workspaces failed ({error:#}); left workspaces alone");
            return Vec::new();
        }
    };
    let mut hashes = Vec::new();
    for workspace in &workspaces {
        let Some(id) = workspace["id"].as_str() else {
            continue;
        };
        let Some(env) = cmux_json(cmux, &["workspace", "env", id, "--json"]) else {
            continue;
        };
        let points_inside = ["DAGQ_QUEUE", "E2E_SHARED"]
            .iter()
            .any(|key| env["env"][key].as_str().is_some_and(left_behind));
        if !points_inside {
            continue;
        }
        let title = workspace["title"].as_str().unwrap_or_default();
        eprintln!("e2e sweep: workspace {id} {title} was left by an e2e");
        close_workspace(cmux, id, "e2e sweep: ");
        // Only a queue that is itself left behind names a group to delete:
        // a live fixture's group stays even if `E2E_SHARED` matched.
        if let Some(queue) = env["env"]["DAGQ_QUEUE"].as_str()
            && left_behind(queue)
            && let Some(hash) = Path::new(queue).parent().and_then(Path::file_name)
        {
            let hash = hash.to_string_lossy().into_owned();
            if !hashes.contains(&hash) {
                hashes.push(hash);
            }
        }
    }
    hashes
}

pub(crate) fn try_listed_group(cmux: &Path, external_id: &str) -> Option<Value> {
    let list = cmux_query(
        cmux,
        &["--json", "--id-format", "uuids", "workspace-group", "list"],
    )
    .ok()?;
    list["groups"]
        .as_array()?
        .iter()
        .find(|group| group["external_id"] == external_id)
        .cloned()
}

fn cmux_json(cmux: &Path, args: &[&str]) -> Option<Value> {
    let output = Command::new(cmux).args(args).bounded_output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Not ignored: these need no cmux.
#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep closes a workspace for its env only when the env points
    /// into a `$TMPDIR/.tmp*` directory that is gone. A live fixture's
    /// directory exists, and the production queue's `DAGQ_QUEUE` is under
    /// the data home, so neither is ever a target.
    #[test]
    fn the_sweep_leaves_live_and_production_queues_alone() {
        let roots = temp_roots();
        let live = tempfile::tempdir().unwrap();
        let gone = tempfile::tempdir().unwrap().path().to_owned();
        let queue = |dir: &Path| {
            dir.join("data/dagq/0123abcd/queue.db")
                .to_string_lossy()
                .into_owned()
        };
        assert!(in_vanished_temp_dir(&queue(&gone), &roots));
        let real = gone
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join(gone.file_name().unwrap());
        assert!(in_vanished_temp_dir(&queue(&real), &roots));
        assert!(!in_vanished_temp_dir(&queue(live.path()), &roots));
        let home = env::var("HOME").unwrap();
        let production = format!("{home}/.local/share/dagq/77067154921b9014/queue.db");
        assert!(!in_vanished_temp_dir(&production, &roots));
        // Only a `.tmp*` directory right under `$TMPDIR` is a fixture's.
        let other = env::temp_dir().join("not-a-fixture-349/queue.db");
        assert!(!in_vanished_temp_dir(other.to_str().unwrap(), &roots));
        assert!(!in_vanished_temp_dir("", &roots));
    }

    /// Every `workspace_id` in the JSON of `up` or `plan` is recorded, at any
    /// depth and once; output that is not JSON gives up its UUIDs.
    #[test]
    fn the_guard_records_every_workspace_an_output_names() {
        let mut guard = WorkspaceGuard {
            cmux: "cmux".into(),
            ids: Vec::new(),
        };
        let a = "7D1F3A52-4C2B-4C0E-9A0B-1F2E3D4C5B6A";
        let b = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
        let up = serde_json::json!({
            "supervisor": {"outcome": "started", "workspace_id": a, "pid": 1},
            "inbox": {"outcome": "created", "workspace_id": b},
            "down": {"supervisor_workspaces": [{"workspace_id": a.to_lowercase()}]},
        });
        guard.record_opened(up.to_string().as_bytes());
        let mut ids: Vec<String> = guard.ids.iter().map(|id| id.to_lowercase()).collect();
        ids.sort();
        assert_eq!(ids, vec![b.to_owned(), a.to_lowercase()]);
        guard.ids.clear();
        guard.record_opened(format!("{{\"inbox\": {{\"workspace_id\": \"{b}\"").as_bytes());
        assert_eq!(guard.ids, vec![b.to_owned()]);
        guard.ids.clear();
        guard.record_opened(format!("workspace-{a} opened\nworkspace:{b}").as_bytes());
        assert_eq!(guard.ids, vec![a.to_owned(), b.to_owned()]);
        guard.ids.clear();
    }
}
