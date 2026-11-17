//! Best-effort cleanup before the first landing command, never between retries.
use std::path::Path;

use serde_json::{Value, json};

use super::{EntryKind, RunFiles};
use crate::domain::toolchain::NonIncrementalBuild;

/// Clear the profiles' worktree-relative outputs while holding compiler locks.
/// A container verification gets a fresh clone and a uniquely named, empty
/// build-output volume, so its corresponding paths outside the worktree have
/// no previous incremental outputs to remove. The worker's worktree is still
/// cleaned here; the environment applies to the commands in either location.
/// Missing outputs are successful no-ops. Reading, measuring and removing
/// failures are reported without stopping verification.
pub(crate) fn remove(
    files: &dyn RunFiles,
    worktree: &Path,
    builds: &[NonIncrementalBuild],
) -> Vec<Value> {
    let mut outputs = Vec::new();
    for build in builds {
        for relative in &build.dirs {
            let dir = worktree.join(relative);
            let mut retained = Vec::new();
            let mut failures = Vec::new();
            let mut measure = || match files.tree_size(&dir) {
                Ok(bytes) => Some(bytes.unwrap_or(0)),
                Err(error) => {
                    failures.push(json!({"path": dir, "error": error.to_string()}));
                    None
                }
            };
            let before = measure();
            clear(files, &dir, build, &mut retained, &mut failures);
            let after = match files.tree_size(&dir) {
                Ok(bytes) => Some(bytes.unwrap_or(0)),
                Err(error) => {
                    failures.push(json!({"path": dir, "error": error.to_string()}));
                    None
                }
            };
            outputs.push(json!({
                "dir": dir,
                "bytes": before.zip(after).map(|(b, a)| b.saturating_sub(a)),
                "retained": retained,
                "failures": failures,
            }));
        }
    }
    outputs
}

fn clear(
    files: &dyn RunFiles,
    path: &Path,
    build: &NonIncrementalBuild,
    retained: &mut Vec<Value>,
    failures: &mut Vec<Value>,
) {
    let result = (|| -> std::io::Result<()> {
        let kind = match files.entry_kind(path) {
            Ok(kind) => kind,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let lock = (build.lock_for)(path);
        let _guard = match lock.as_ref() {
            Some(lock) => {
                // A worker may leave a link where the compiler's lock belongs.
                // Never open that link or another non-file as a lock.
                match files.entry_kind(lock) {
                    Ok(EntryKind::File) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "compiler lock is not a regular file",
                        ));
                    }
                    Err(error) => return Err(error),
                }
                match files.try_lock(lock)? {
                    Some(guard) => Some(guard),
                    None => {
                        retained.push(json!({"path": path, "reason": "locked", "lock": lock}));
                        return Ok(());
                    }
                }
            }
            None => None,
        };
        if kind == EntryKind::Dir {
            if lock.is_some() {
                files.remove_dir_all(path)?;
            } else {
                let mut entries = files.read_dir(path)?;
                entries.sort();
                for entry in entries {
                    clear(files, &entry, build, retained, failures);
                }
                files.remove_empty_dir(path)?;
            }
        } else {
            files.remove_file(path)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        failures.push(json!({"path": path, "error": error.to_string()}));
    }
}
