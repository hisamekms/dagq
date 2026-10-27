//! What the queue fixtures of the e2e and `tests/it` leave on disk, and how
//! it is cleaned up again: the owner lock of a fixture's directory, the
//! sweep that cleans up after earlier test processes that died before their
//! `TempDir` could drop (a SIGTERM or SIGKILL of nextest's timeout or of a
//! cancelled check), and a removal that outlasts a late write into the
//! directory.
use super::Bounded;
use std::{
    collections::HashSet,
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex, PoisonError},
    thread,
    time::{Duration, Instant},
};

/// The file in a fixture's temporary directory that its test process holds an
/// exclusive `flock` on for as long as the fixture lives. The lock goes away
/// with the process however it ends, SIGKILL included, so a directory whose
/// owner file can be locked by someone else belongs to a dead test.
pub const OWNER_FILE: &str = "fixture-owner";
/// The owner file of an e2e fixture of a binary from before [`OWNER_FILE`],
/// locked as it is.
const E2E_OWNER_FILE: &str = "e2e-owner";
/// A fixture directory without an owner file (one of the shapes in
/// [`unmarked_fixture`]) is swept only once it is this old.
const UNMARKED_SWEEP_AGE: Duration = Duration::from_secs(60 * 60);

/// [`owned_temp_dir_in`] in `$TMPDIR`.
pub fn owned_temp_dir() -> (tempfile::TempDir, fs::File) {
    owned_temp_dir_in(&env::temp_dir())
}

/// [`owned_temp_dir_in`] in `/tmp`, for a fixture that must be short and
/// under no `runs/` directory ([`super::short_tempdir`]).
pub fn owned_short_temp_dir() -> (tempfile::TempDir, fs::File) {
    owned_temp_dir_in(Path::new(super::SHORT_TEMP_ROOT))
}

/// A fresh temporary directory in `root` for a queue fixture, owned by this
/// process ([`claim_fixture_dir`]): the lock file is held while the fixture
/// keeps it. The first one of a process in each `root` sweeps first what
/// dead test processes left there ([`sweep_abandoned_fixtures_in`]); the
/// others wait for that sweep.
pub fn owned_temp_dir_in(root: &Path) -> (tempfile::TempDir, fs::File) {
    static SWEPT: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
    {
        let mut swept = SWEPT.lock().unwrap_or_else(PoisonError::into_inner);
        if swept.insert(root.to_owned()) {
            sweep_abandoned_fixtures_in(root);
        }
    }
    let dir = tempfile::tempdir_in(root).unwrap();
    let owner = claim_fixture_dir(dir.path());
    (dir, owner)
}

/// Remove `dir` and keep removing it while something writes into it as it
/// goes (a session wrapper's thread that writes its idle marker as the stub
/// it ran is killed): one `remove_dir_all` that meets an entry made after it
/// read the directory fails with the rest left (the entry's parents
/// included). Gone once this returns, unless a writer kept at it for a
/// second.
pub fn remove_fixture_dir(dir: &Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) if Instant::now() >= deadline => {
                eprintln!("removing {} failed: {error}", dir.display());
                return;
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Take ownership of a fresh fixture directory: lock the owner file under a
/// temporary name and rename it into place, so a concurrent sweep never sees
/// an owner file that is not yet locked.
pub fn claim_fixture_dir(dir: &Path) -> fs::File {
    let staging = dir.join(format!("{OWNER_FILE}.tmp"));
    let mut file = fs::File::create(&staging).unwrap();
    file.lock().unwrap();
    use std::io::Write;
    writeln!(file, "{}", std::process::id()).unwrap();
    fs::rename(&staging, dir.join(OWNER_FILE)).unwrap();
    file
}

/// Clean up what earlier tests left behind in this process's temporary
/// directory when their process died before its `TempDir` could drop
/// (SIGTERM, SIGKILL): see [`sweep_abandoned_fixtures_in`].
pub fn sweep_abandoned_fixtures() {
    sweep_abandoned_fixtures_in(&env::temp_dir());
}

/// For a fixture directory in `root` that is still there but abandoned, the
/// processes running from or on it, and the directory itself. A directory
/// counts as abandoned only when its owner lock is free (the owner process
/// is gone), or, without an owner file, when it has one of the fixtures'
/// shapes ([`unmarked_fixture`]) and is older than [`UNMARKED_SWEEP_AGE`].
/// The sweep holds that lock while it works, so concurrent sweeps never take
/// the same directory.
pub fn sweep_abandoned_fixtures_in(root: &Path) {
    let mut abandoned = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            // A link is no fixture's, whatever it points at: the sweep
            // writes, signals and removes nothing out of `root`.
            if !entry.file_name().to_string_lossy().starts_with(".tmp")
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
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
        eprintln!("fixture sweep: {} was left by a dead test", dir.display());
    }
    if !abandoned.is_empty() {
        kill_processes_inside(&inside);
    }
    for (dir, lock) in abandoned {
        let removed = fs::remove_dir_all(&dir).or_else(|_| {
            make_removable(&dir);
            fs::remove_dir_all(&dir)
        });
        match removed {
            Ok(()) => eprintln!("fixture sweep: removed {}", dir.display()),
            Err(error) => eprintln!("fixture sweep: removing {} failed: {error}", dir.display()),
        }
        drop(lock);
    }
}

/// Give the owner back the bits to remove from `dir` and the directories
/// under it, which a killed test may have taken away to inject a failure
/// ([`super::set_mode`]), so that no later sweep fails on them again.
fn make_removable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mode = metadata.permissions().mode() | 0o700;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            make_removable(&entry.path());
        }
    }
}

/// The locked owner file of `dir`, a directory and no link, when it is a
/// fixture directory whose owner is gone, `None` when it is live, not a
/// fixture, or taken by another sweep. An owner file that is a link is
/// neither opened nor made.
fn claim_abandoned(dir: &Path) -> Option<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let marked = [OWNER_FILE, E2E_OWNER_FILE]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|owner| fs::symlink_metadata(owner).is_ok());
    let (owner, file) = if let Some(owner) = marked {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&owner)
            .ok()?;
        (owner, file)
    } else {
        if !unmarked_fixture(dir) {
            return None;
        }
        let age = fs::metadata(dir).ok()?.modified().ok()?.elapsed().ok()?;
        if age < UNMARKED_SWEEP_AGE {
            return None;
        }
        let owner = dir.join(OWNER_FILE);
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&owner)
            .ok()?;
        (owner, file)
    };
    file.try_lock().ok()?;
    // A lock freed as its owner removed the directory is no dead owner's.
    fs::symlink_metadata(&owner).is_ok().then_some(file)
}

/// Whether `dir`, which has no owner file, has the shape of a fixture of a
/// binary from before the owner file: an e2e's (its stub agent and queue
/// data home), a `tests/it` queue fixture's (its repository beside its queue
/// or its runs), a container queue's (its repository, queue data home and
/// fake podman), or what a session wrapper's late idle marker made again of
/// one (only `runs/<run>/idle.json`). Links count as neither files nor
/// directories.
fn unmarked_fixture(dir: &Path) -> bool {
    let e2e = real_file(&dir.join("claude-stub")) && real_dir(&dir.join("data"));
    let it = real_dir(&dir.join("repo's directory"))
        && (real_file(&dir.join("queue's data.db")) || real_dir(&dir.join("runs")));
    let container = ["repo", "data", "podman"]
        .iter()
        .all(|name| real_dir(&dir.join(name)));
    e2e || it || container || only_idle_markers(dir)
}

/// Whether `path` is a directory and no link.
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Whether `path` is a regular file and no link.
fn real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Whether `dir` holds only `runs`, each of whose entries is a directory
/// holding only `idle.json`, none of them a link.
fn only_idle_markers(dir: &Path) -> bool {
    let names = |dir: &Path| -> Option<Vec<_>> {
        fs::read_dir(dir)
            .ok()?
            .map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect()
    };
    if names(dir).is_none_or(|names| names != ["runs"]) {
        return false;
    }
    let runs = dir.join("runs");
    real_dir(&runs)
        && names(&runs).is_some_and(|runs_names| {
            !runs_names.is_empty()
                && runs_names.iter().all(|run| {
                    let run = runs.join(run);
                    real_dir(&run)
                        && names(&run).is_some_and(|names| names == ["idle.json"])
                        && real_file(&run.join("idle.json"))
                })
        })
}

/// Whether the process `pid` exists.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks; it takes no pointer.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
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
        eprintln!("fixture sweep: ps failed; left processes alone");
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
            eprintln!(
                "fixture sweep: terminating process {pid}: {}",
                argv.join(" ")
            );
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
        eprintln!("fixture sweep: killing process {pid}, still alive after SIGTERM");
        // SAFETY: as above.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
    }
}
