//! The binary that changes a queue's schema (ADR-t3102-2 decisions 2 and
//! 3): a queue names one binary by its path in [`RECORD_FILE`] next to its
//! database, and only the binary at that path applies the migrations the
//! queue lacks (`migrate`, and the compatible ones `up` applies). `init`
//! names the binary that created the queue, so a throwaway queue is its
//! creator's; `install` (and the automatic update, which runs it) names the
//! path it put the binary at. The build is not compared: queues of several
//! repositories share one fixed binary, and an `install` for one of them
//! replaces it for all. The new build `install` migrates the queue with
//! before it is in place is let through by a [`Grant`] naming it, held by
//! the install's process for that one `migrate`. Any other binary, a
//! development build or a run's `runner` copy, is refused before it
//! changes anything; reading the schema (`migrate --check`) and opening the
//! queue are not judged here. A queue with no record yet takes any binary
//! until an `install` names one.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The record of the queue's binary, in the database's directory.
pub const RECORD_FILE: &str = "queue-binary.json";
/// The grant of one `migrate` to the build `install` puts in place, in the
/// database's directory while that `migrate` runs.
pub const GRANT_FILE: &str = "queue-binary-grant.json";

/// The binary a queue names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBinary {
    /// The file it runs from, canonical when it exists.
    pub path: PathBuf,
}

impl QueueBinary {
    pub fn new(path: &Path) -> Self {
        Self {
            path: canonical(path),
        }
    }
}

/// The binary an `install` lets migrate, and the install's process: a
/// grant whose process is gone (an install killed mid-way) grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantRecord {
    path: PathBuf,
    pid: u32,
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn beside(db: &Path, name: &str) -> PathBuf {
    db.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

fn read_json<T: serde::de::DeserializeOwned>(file: &Path) -> Result<Option<T>> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("read {}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", file.display())),
    }
}

fn write_json(file: &Path, value: &impl Serialize) -> Result<()> {
    let staged = file.with_extension(format!("json.{}", std::process::id()));
    fs::write(&staged, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", staged.display()))?;
    fs::rename(&staged, file).with_context(|| format!("write {}", file.display()))
}

/// The binary the queue at `db` names; `None` when it names none yet.
pub fn read(db: &Path) -> Result<Option<QueueBinary>> {
    read_json(&beside(db, RECORD_FILE))
}

/// Name `binary` as the queue at `db`'s binary.
pub fn write(db: &Path, binary: &QueueBinary) -> Result<()> {
    write_json(&beside(db, RECORD_FILE), binary)
}

/// A grant of the queue's `migrate` to one binary for as long as this
/// process holds it, removed when dropped.
#[derive(Debug)]
pub struct Grant {
    file: PathBuf,
}

impl Grant {
    /// Let `binary` change the schema of the queue at `db` until the grant
    /// is dropped or this process ends.
    pub fn to(db: &Path, binary: &Path) -> Result<Self> {
        let file = beside(db, GRANT_FILE);
        write_json(
            &file,
            &GrantRecord {
                path: canonical(binary),
                pid: std::process::id(),
            },
        )?;
        Ok(Self { file })
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
    }
}

/// Whether the binary at `running` (canonical) may change the schema of a
/// queue that names `named` and whose live `install` grants its `migrate`
/// to `granted`: it may when it is the granted binary or the named one, or
/// the queue names none. The refusal says which binary to use instead.
pub fn judge(named: Option<&QueueBinary>, granted: Option<&Path>, running: &Path) -> Result<()> {
    if granted == Some(running) {
        return Ok(());
    }
    let Some(named) = named else {
        return Ok(());
    };
    if named.path != running {
        bail!(
            "{} is not the queue's binary ({}): only the queue's binary changes its schema, so a \
development build or a run's copy cannot raise it; nothing was changed. Run `{} migrate` (or its \
`up`), or put a new build in place with `dagq install`",
            running.display(),
            named.path.display(),
            named.path.display()
        );
    }
    Ok(())
}

/// [`judge`] the binary at `executable` for the queue at `db`, reading its
/// record and a grant whose install still runs (`alive`).
pub fn check(db: &Path, executable: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
    let granted = read_json::<GrantRecord>(&beside(db, GRANT_FILE))?
        .filter(|grant| alive(grant.pid))
        .map(|grant| grant.path);
    judge(
        read(db)?.as_ref(),
        granted.as_deref(),
        &canonical(executable),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary(path: &str) -> QueueBinary {
        QueueBinary { path: path.into() }
    }

    #[test]
    fn only_the_named_binary_or_the_granted_one_changes_the_schema() {
        let fixed = binary("/home/u/.local/bin/dagq");
        assert!(judge(Some(&fixed), None, &fixed.path).is_ok());
        // A development build, or a run's runner copy, at another path.
        let dev = Path::new("/src/dagq/target/debug/dagq");
        let error = format!("{:#}", judge(Some(&fixed), None, dev).unwrap_err());
        assert!(error.contains("is not the queue's binary"), "{error}");
        assert!(
            error.contains("Run `/home/u/.local/bin/dagq migrate`"),
            "{error}"
        );
        // The build `install` migrates with before it is in place.
        let next = Path::new("/q/update/target/release/dagq");
        assert!(judge(Some(&fixed), Some(next), next).is_ok());
        assert!(judge(Some(&fixed), Some(next), dev).is_err());
        // A queue that names no binary yet.
        assert!(judge(None, None, dev).is_ok());
    }

    #[test]
    fn the_record_and_the_grant_of_a_live_install_live_beside_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let exe = dir.path().join("dagq");
        fs::write(&exe, "").unwrap();
        let other = dir.path().join("other");
        fs::write(&other, "").unwrap();
        let live = |_: u32| true;
        assert_eq!(read(&db).unwrap(), None);
        assert!(check(&db, &other, &live).is_ok());
        write(&db, &QueueBinary::new(&exe)).unwrap();
        assert_eq!(read(&db).unwrap(), Some(QueueBinary::new(&exe)));
        assert!(check(&db, &exe, &live).is_ok());
        assert!(check(&db, &other, &live).is_err());
        {
            let _grant = Grant::to(&db, &other).unwrap();
            assert!(check(&db, &other, &live).is_ok());
            // The grant of an install whose process is gone.
            assert!(check(&db, &other, &|_| false).is_err());
        }
        assert!(!dir.path().join(GRANT_FILE).exists());
        assert!(check(&db, &other, &live).is_err());
    }
}
