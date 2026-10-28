//! The sessions of rustc's incremental compilation a build no longer reads
//! ([`super::Rust`]'s [`ToolchainProfile::stale_build_entries`]).
//!
//! cargo gives rustc a directory per crate (each library, binary and test
//! target) under `<profile>/incremental/<crate>-<hash>/` of its target
//! directory, and rustc keeps there a directory per session of it, with a
//! lock file beside it. The rules below are those of
//! `compiler/rustc_incremental/src/persist/fs.rs`, the same in rustc
//! 1.98.1 and 1.99.1; the reading of it and the measure of what removing
//! the old sessions does to the next build are in
//! docs/plans/incremental-session-removal-spike.md, 「安全性の根拠」:
//!
//! - A session works in `s-<T>-<R>-working` (`generate_session_dir_path`),
//!   `<T>` the time it began, in microseconds written in base 36 at a fixed
//!   length, and `<R>` a random number, also at a fixed length; so the
//!   order of the names' `<T>` is the order of the times. A build that ends
//!   well renames it `s-<T>-<R>-<SVH>` (`finalize_session_directory`): a
//!   finished session.
//! - Its lock is `s-<T>-<R>.lock` (`lock_file_path`), the same for the
//!   working directory and the finished one.
//! - A build reads the newest finished session alone
//!   (`find_source_directory_in_iter`), and moves to an older one only when
//!   it cannot copy that one, which on a target no build runs in it can.
//! - At its end it removes the finished sessions but the newest one,
//!   leaving its own aside (`garbage_collect_session_directories`): after
//!   a build, the directory holds its own session and the one before.
//!   The cleanup removing the one before frees room only until the next
//!   build, which leaves its own one before again.
//!
//! What is stale, then, is every finished session older than the newest
//! one, with its lock. A working session, a lock with the `s-<T>-<R>` of a
//! working session, and a lock with no session are left to rustc's own
//! collection, as is anything else. The cleanup takes no lock of rustc's:
//! it removes them only where no build runs.
use std::collections::HashSet;
use std::path::Path;

use super::super::StaleEntries;

/// The directory under a profile's directory of the target where cargo
/// keeps the crates' incremental directories.
const INCREMENTAL: &str = "incremental";
/// The suffix of the session a build works in.
const WORKING: &str = "working";
/// The suffix of a session's lock file.
const LOCK: &str = ".lock";

/// The stale sessions of `target`, a cargo target directory: under each
/// `<profile>/incremental/<crate>/`, the [`stale_sessions`] of it.
pub(super) fn stale_entries(
    target: &Path,
    read_dir: &dyn Fn(&Path) -> Vec<String>,
) -> Vec<StaleEntries> {
    let mut stale = Vec::new();
    for profile in sorted(read_dir(target)) {
        let incremental = target.join(profile).join(INCREMENTAL);
        for krate in sorted(read_dir(&incremental)) {
            let dir = incremental.join(krate);
            let remove = stale_sessions(&read_dir(&dir));
            if !remove.is_empty() {
                stale.push(StaleEntries { dir, remove });
            }
        }
    }
    stale
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

/// A session's directory name `s-<T>-<R>-<last>` read: its `s-<T>-<R>`,
/// `<T>` and `<last>`. `None` for any other name (a lock included, whose
/// name has a hyphen less).
fn session(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix("s-")?;
    let mut parts = rest.split('-');
    let (time, random, last) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || [time, random, last].iter().any(|part| part.is_empty()) {
        return None;
    }
    Some((&name[..name.len() - last.len() - 1], time, last))
}

/// Of `names`, the entries of one crate's incremental directory, the
/// finished sessions older than the newest finished one and their locks
/// that are there, in the order of `names` sorted: see the module's doc.
pub(super) fn stale_sessions(names: &[String]) -> Vec<String> {
    let present: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut finished: Vec<(&str, &str, &str)> = Vec::new();
    let mut working: HashSet<&str> = HashSet::new();
    for name in names {
        let Some((prefix, time, last)) = session(name) else {
            continue;
        };
        if last == WORKING {
            working.insert(prefix);
        } else if !last.contains('.') {
            finished.push((time, name, prefix));
        }
    }
    finished.sort_unstable();
    finished.pop();
    let mut stale = Vec::new();
    for (_, name, prefix) in finished {
        stale.push(name.to_owned());
        let lock = format!("{prefix}{LOCK}");
        if present.contains(lock.as_str()) && !working.contains(prefix) {
            stale.push(lock);
        }
    }
    stale.sort();
    stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::toolchain::ToolchainProfile;
    use crate::domain::toolchain::rust::RUST;
    use std::path::PathBuf;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The finished sessions older than the newest, by the time in their
    /// names whatever the order listed, go with their locks; the newest
    /// and its lock stay, and so do a working session, the lock of its
    /// `s-<T>-<R>` (also when a finished session had the same one), a
    /// lock with no session and any other entry. A lock not there is not
    /// named.
    #[test]
    fn only_the_finished_sessions_older_than_the_newest_and_their_locks_are_stale() {
        let crate_dir = names(&[
            "s-00b-r2-svh2",
            "s-00b-r2.lock",
            "s-00a-r1-svh1",
            "s-00a-r1.lock",
            "s-00c-r3-svh3",
            "s-00c-r3.lock",
            "s-009-r0-svh0",
            "s-00d-r4-working",
            "s-00d-r4.lock",
            "s-008-r9.lock",
            "s-007-r8-working",
            "s-007-r8-old",
            "s-007-r8.lock",
            "query-cache.bin",
            "s-00e-r5-svh5-extra",
            "s-00f",
        ]);
        assert_eq!(
            stale_sessions(&crate_dir),
            names(&[
                "s-007-r8-old",
                "s-009-r0-svh0",
                "s-00a-r1-svh1",
                "s-00a-r1.lock",
                "s-00b-r2-svh2",
                "s-00b-r2.lock",
            ])
        );
    }

    /// One finished session, or none, leaves nothing stale.
    #[test]
    fn a_lone_finished_session_is_not_stale() {
        assert!(stale_sessions(&names(&["s-00a-r1-svh1", "s-00a-r1.lock"])).is_empty());
        assert!(stale_sessions(&names(&["s-00a-r1-working", "s-00a-r1.lock"])).is_empty());
        assert!(stale_sessions(&names(&["s-00a-r1.lock"])).is_empty());
        assert!(stale_sessions(&[]).is_empty());
    }

    /// The profile looks under each profile's `incremental` of the target,
    /// at each crate's directory, and names only the crates with something
    /// stale; a profile directory without `incremental` (a file, `build`,
    /// `deps`) has nothing.
    #[test]
    fn the_rust_profile_names_the_stale_sessions_of_each_crate_of_each_profile() {
        let read_dir = |dir: &Path| -> Vec<String> {
            names(match dir.to_str().unwrap() {
                "/w/target" => &["release", "debug", "CACHEDIR.TAG"],
                "/w/target/debug/incremental" => &["it-2", "dagq-1"],
                "/w/target/debug/incremental/dagq-1" => {
                    &["s-00a-r1-x", "s-00a-r1.lock", "s-00b-r2-y", "s-00b-r2.lock"]
                }
                "/w/target/debug/incremental/it-2" => &["s-00a-r1-x", "s-00a-r1.lock"],
                "/w/target/release/incremental" => &["dagq-3"],
                "/w/target/release/incremental/dagq-3" => &["s-001-r-x", "s-002-r-y"],
                _ => &[],
            })
        };
        let incremental = PathBuf::from("/w/target/debug/incremental");
        assert_eq!(
            RUST.stale_build_entries(Path::new("/w/target"), &read_dir),
            [
                StaleEntries {
                    dir: incremental.join("dagq-1"),
                    remove: names(&["s-00a-r1-x", "s-00a-r1.lock"]),
                },
                StaleEntries {
                    dir: PathBuf::from("/w/target/release/incremental/dagq-3"),
                    remove: names(&["s-001-r-x"]),
                },
            ]
        );
        assert!(
            RUST.stale_build_entries(Path::new("/w/none"), &read_dir)
                .is_empty()
        );
    }
}
