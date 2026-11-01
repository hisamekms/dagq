//! The binary that runs a queue's service (ADR-t3102-2 decision 3): a
//! queue names one binary by its path in [`RECORD_FILE`] next to its
//! database, and only the binary at that path starts or replaces the
//! queue's service (`up`, `service start`, `service serve` and a
//! supervisor's watch), the one process that opens the queue and applies
//! its migrations (decisions 1 and 2). `init` names the binary that created
//! the queue, so a throwaway queue is its creator's; `install` (and the
//! automatic update, which runs it) names the path it put the binary at.
//! The build is not compared: queues of several repositories share one
//! fixed binary, and an `install` for one of them replaces it for all. The
//! new build whose service `install` has migrate the queue before the
//! build is in place is let through by a [`Grant`] naming it, held by the
//! install's process for that one start. Any other binary, a development
//! build or a run's `runner` copy, is refused before it starts anything.
//! A queue with no record yet (one made before the record, or by a library
//! call) is named by `install` (the automatic update's included), or by
//! the first binary that starts its service while none runs, recorded once
//! it holds the queue: from then on, it is that binary's alone. No start
//! stops or replaces a running service of such a queue without an
//! install's grant ([`refuses_takeover`]), so a development build cannot
//! stop the service running there and name itself.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The record of the queue's binary, in the database's directory.
pub const RECORD_FILE: &str = "queue-binary.json";
/// The grant of the migrations to the build `install` puts in place, in
/// the database's directory while its service starts.
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

/// Whether the binary at `running` (canonical) may start the service of
/// a queue that names `named` and whose live `install` grants its start to
/// `granted`: it may when it is the granted binary or the named one. The
/// refusal names the queue's binary to use instead.
fn judge(named: &QueueBinary, granted: Option<&Path>, running: &Path) -> Result<()> {
    if granted == Some(running) || named.path == running {
        return Ok(());
    }
    bail!(
        "{} is not the queue's binary ({}): only the queue's binary starts or replaces the \
queue's service, which alone opens the queue and changes its schema, so a development build or a \
run's copy cannot; nothing was started. Run `{} up` (or its `service start`), or put a new build \
in place with `dagq install`",
        running.display(),
        named.path.display(),
        named.path.display()
    );
}

/// [`judge`] the binary at `executable` for the queue at `db`, reading its
/// record and a grant whose install still runs (`alive`). A queue that
/// names no binary yet is named `executable`'s here, before it starts
/// anything: the record is made for this process alone, so of two that
/// race, the one that comes second is judged against the first's.
pub fn check(db: &Path, executable: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
    let running = canonical(executable);
    let granted = read_json::<GrantRecord>(&beside(db, GRANT_FILE))?
        .filter(|grant| alive(grant.pid))
        .map(|grant| grant.path);
    let named = match read(db)? {
        Some(named) => named,
        None if granted.is_some() => return judge_granted(granted.as_deref(), &running),
        None => match claim(db, &running)? {
            Some(named) => named,
            None => return Ok(()),
        },
    };
    judge(&named, granted.as_deref(), &running)
}

/// Refuse the binary at `executable` for the queue at `db` as [`check`]
/// would, naming nothing: a queue that names no binary refuses none here,
/// for the start that follows to name its own. What a starter judges
/// before it stops a running service, as it may start a `dagq` that is not
/// the binary itself (a script that runs it).
pub fn refuses(db: &Path, executable: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
    let running = canonical(executable);
    let granted = read_json::<GrantRecord>(&beside(db, GRANT_FILE))?
        .filter(|grant| alive(grant.pid))
        .map(|grant| grant.path);
    match read(db)? {
        Some(named) => judge(&named, granted.as_deref(), &running),
        None => judge_granted(granted.as_deref(), &running),
    }
}

/// Refuse the binary at `executable` a start that stops or takes over the
/// running service of the queue at `db`: as [`refuses`] on a queue that
/// names a binary, and on one that names none unless a live install
/// grants it to `executable`, as only `install` names the binary of a
/// queue whose service runs (ADR-t3102-2 decision 3).
pub fn refuses_takeover(db: &Path, executable: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
    let running = canonical(executable);
    let granted = read_json::<GrantRecord>(&beside(db, GRANT_FILE))?
        .filter(|grant| alive(grant.pid))
        .map(|grant| grant.path);
    match (read(db)?, granted) {
        (Some(named), granted) => judge(&named, granted.as_deref(), &running),
        (None, Some(granted)) => judge_granted(Some(&granted), &running),
        (None, None) => bail!(
            "the queue at {} names no binary yet (no {RECORD_FILE} next to it) and its service \
runs: no start stops or replaces that service, so {} started nothing. `dagq install` names the \
queue's binary; with no service running, the first start names its own",
            db.display(),
            running.display()
        ),
    }
}

/// A queue with no record whose `install` grants a start: only the granted
/// binary starts it, and the install names its binary after.
fn judge_granted(granted: Option<&Path>, running: &Path) -> Result<()> {
    match granted {
        Some(granted) if granted == running => Ok(()),
        Some(granted) => judge(
            &QueueBinary {
                path: granted.to_path_buf(),
            },
            None,
            running,
        ),
        None => Ok(()),
    }
}

/// Name `running` the binary of the queue at `db`, which names none:
/// `None` when this process made the record, else the record another made
/// first.
fn claim(db: &Path, running: &Path) -> Result<Option<QueueBinary>> {
    let file = beside(db, RECORD_FILE);
    let binary = QueueBinary {
        path: running.to_path_buf(),
    };
    let staged = file.with_extension(format!("json.{}", std::process::id()));
    fs::write(&staged, serde_json::to_vec_pretty(&binary)?)
        .with_context(|| format!("write {}", staged.display()))?;
    // A hard link is made only where no file is: the first one stays.
    let linked = fs::hard_link(&staged, &file);
    let _ = fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read(db),
        Err(error) => Err(error).with_context(|| format!("write {}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary(path: &str) -> QueueBinary {
        QueueBinary { path: path.into() }
    }

    #[test]
    fn only_the_named_binary_or_the_granted_one_starts_the_service() {
        let fixed = binary("/home/u/.local/bin/dagq");
        assert!(judge(&fixed, None, &fixed.path).is_ok());
        // A development build, or a run's runner copy, at another path.
        let dev = Path::new("/src/dagq/target/debug/dagq");
        let error = format!("{:#}", judge(&fixed, None, dev).unwrap_err());
        assert!(error.contains("is not the queue's binary"), "{error}");
        assert!(
            error.contains("Run `/home/u/.local/bin/dagq up`"),
            "{error}"
        );
        // The build `install` migrates with before it is in place.
        let next = Path::new("/q/update/target/release/dagq");
        assert!(judge(&fixed, Some(next), next).is_ok());
        assert!(judge(&fixed, Some(next), dev).is_err());
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

    /// A queue that names no binary is named by the first binary that
    /// starts its service, and from then on refuses any other; while an
    /// install's grant lives, only the granted binary starts it.
    #[test]
    fn a_queue_with_no_record_is_the_first_starters() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        for exe in [&first, &second] {
            fs::write(exe, "").unwrap();
        }
        let live = |_: u32| true;
        {
            let _grant = Grant::to(&db, &second).unwrap();
            assert!(check(&db, &first, &live).is_err());
            assert!(check(&db, &second, &live).is_ok());
            assert_eq!(read(&db).unwrap(), None);
        }
        assert!(check(&db, &first, &live).is_ok());
        assert_eq!(read(&db).unwrap(), Some(QueueBinary::new(&first)));
        let error = format!("{:#}", check(&db, &second, &live).unwrap_err());
        assert!(error.contains("is not the queue's binary"), "{error}");
        assert!(check(&db, &first, &live).is_ok());
        // A record another process made first is kept.
        assert_eq!(
            claim(&db, &canonical(&second)).unwrap(),
            Some(QueueBinary::new(&first))
        );
    }

    /// A running service of a queue that names no binary is stopped or
    /// replaced by no start but the one an install grants; once the queue
    /// names a binary, by that binary's.
    #[test]
    fn a_running_service_of_a_queue_with_no_record_is_left_to_install() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("queue.db");
        let fixed = dir.path().join("fixed");
        let dev = dir.path().join("dev");
        for exe in [&fixed, &dev] {
            fs::write(exe, "").unwrap();
        }
        let live = |_: u32| true;
        for exe in [&fixed, &dev] {
            let error = format!("{:#}", refuses_takeover(&db, exe, &live).unwrap_err());
            assert!(error.contains("names no binary yet"), "{error}");
            // A start while no service runs is not refused.
            assert!(refuses(&db, exe, &live).is_ok());
        }
        {
            let _grant = Grant::to(&db, &fixed).unwrap();
            assert!(refuses_takeover(&db, &fixed, &live).is_ok());
            assert!(refuses_takeover(&db, &dev, &live).is_err());
            // The grant of an install whose process is gone.
            assert!(refuses_takeover(&db, &fixed, &|_| false).is_err());
        }
        assert_eq!(read(&db).unwrap(), None);
        write(&db, &QueueBinary::new(&fixed)).unwrap();
        assert!(refuses_takeover(&db, &fixed, &live).is_ok());
        let error = format!("{:#}", refuses_takeover(&db, &dev, &live).unwrap_err());
        assert!(error.contains("is not the queue's binary"), "{error}");
    }
}
