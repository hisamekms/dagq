//! What the e2e tests leave on disk, and how it is cleaned up again: the
//! owner lock of a fixture's directory, and the sweep that cleans up after
//! earlier e2e processes that died before their `TempDir` could drop. No
//! e2e opens anything in cmux (ADR-t2159-1 decision 7).
use super::pid_alive;
use crate::common::Bounded;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

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
/// its `TempDir` could drop (SIGTERM, SIGKILL): for a fixture directory that
/// is still there but abandoned, the processes running from or on it, and
/// the directory itself. A directory counts as abandoned only when its owner
/// lock is free (the owner process is gone), or, without an owner file,
/// when it has the fixture's shape and is older than
/// [`UNMARKED_SWEEP_AGE`]. The sweep holds that lock while it works, so
/// concurrent sweeps never take the same directory.
pub(crate) fn sweep_abandoned_fixtures() {
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
    for (dir, lock) in abandoned {
        match fs::remove_dir_all(&dir) {
            Ok(()) => eprintln!("e2e sweep: removed {}", dir.display()),
            Err(error) => eprintln!("e2e sweep: removing {} failed: {error}", dir.display()),
        }
        drop(lock);
    }
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
