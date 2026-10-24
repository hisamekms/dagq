//! host運用's part of the sweep's removal of what ended jobs left in the
//! queue directory past its retention (ADR-t697-1, [`RetentionConfig`]):
//! the directory of an observation and the copies in `backups/` past the
//! newest few. 計画管理 removes its planners' and plan reviews'
//! directories itself ([`super::planner::remove_expired_dirs`]). The run
//! directories and the observer's cursor stay.

use anyhow::{Context, Result};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{Clock, EntryKind, EventStore, ProcessControl, RunFiles};
use crate::domain::{
    EventId, RunEvent,
    event_kind::{OBSERVE_FINISHED, OBSERVE_STARTED},
    retention::{
        RecordedObservation, RetentionConfig, Swept, ended_observations, observation_name,
    },
};

/// How many observation events one read of the queue takes.
const OBSERVATION_EVENTS_PAGE: usize = 1_000;

/// The directories of the queue host運用's retention reads.
#[derive(Debug, Clone)]
pub struct RetentionDirs {
    /// `observer/`: one directory per observation, by its start, and the
    /// cursor.
    pub observer: PathBuf,
    /// `backups/`: the copies a breaking `migrate` made.
    pub backups: PathBuf,
}

impl RetentionDirs {
    /// The directories beside the queue database `db`; `backups/` beside
    /// the file `db` resolves to, where `migrate` writes its copies.
    pub fn of(db: &Path, files: &dyn RunFiles) -> Self {
        let root = db.parent().unwrap_or(Path::new("."));
        let resolved = files.canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
        Self {
            observer: root.join("observer"),
            backups: resolved.with_file_name("backups"),
        }
    }
}

/// Remove what is past its retention at the clock's now under `config`:
/// the directory of each observation whose newest file was last written at
/// or before [`RetentionConfig::cutoff`] and that is over by the queue's
/// records ([`ended_observations`]: its finish recorded and its process
/// gone; one with no record, or when the records cannot be read, stays),
/// and the copies in `backups/` past the newest
/// [`RetentionConfig::backups`]. Every removal is tried, and what went is
/// returned with the first failure, if any.
pub fn remove_expired(
    events: &dyn EventStore,
    processes: &dyn ProcessControl,
    files: &dyn RunFiles,
    clock: &dyn Clock,
    dirs: &RetentionDirs,
    config: &RetentionConfig,
) -> Swept<anyhow::Error> {
    let cutoff = config.cutoff(clock.now());
    let mut swept = Swept::new();
    // The queue's records are read only for the observations past their
    // time by their files.
    let observations = expired_observations(files, &dirs.observer, cutoff).and_then(|old| {
        if old.is_empty() {
            return Ok(old);
        }
        let events = observation_events(events)?;
        let records: Vec<_> = events
            .iter()
            .filter_map(RecordedObservation::restore)
            .collect();
        let ended = ended_observations(&records, |pid| processes.alive(pid));
        Ok(old
            .into_iter()
            .filter(|dir| {
                dir.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| ended.contains(name))
            })
            .collect())
    });
    match observations {
        Ok(observations) => {
            for dir in observations {
                if real_dir(files, &dir) {
                    remove(files.remove_dir_all(&dir), dir, &mut swept);
                }
            }
        }
        Err(error) => swept.failed(error),
    }
    match old_backups(files, &dirs.backups, config) {
        Ok(copies) => {
            for copy in copies {
                remove(files.remove_file(&copy), copy, &mut swept);
            }
        }
        Err(error) => swept.failed(error),
    }
    swept
}

/// Record what removing `path` came to in `swept`: gone, already gone, or
/// a failure.
fn remove(result: io::Result<()>, path: PathBuf, swept: &mut Swept<anyhow::Error>) {
    match result {
        Ok(()) => swept.removed.push(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            swept.failed(anyhow::Error::new(error).context(format!("remove {}", path.display())))
        }
    }
}

/// The entries of `dir`, none when it is not there.
fn entries(files: &dyn RunFiles, dir: &Path) -> Result<Vec<PathBuf>> {
    match files.read_dir(dir) {
        Ok(entries) => Ok(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", dir.display())),
    }
}

/// Every `observe_started` and `observe_finished` of the queue, oldest
/// first.
fn observation_events(events: &dyn EventStore) -> Result<Vec<RunEvent>> {
    let mut read = Vec::new();
    let mut after = EventId::new(0);
    loop {
        let page = events.events_of_between(
            &[OBSERVE_STARTED, OBSERVE_FINISHED],
            after,
            EventId::new(i64::MAX),
            OBSERVATION_EVENTS_PAGE,
        )?;
        let Some(last) = page.last() else {
            return Ok(read);
        };
        after = last.id;
        let full = page.len() == OBSERVATION_EVENTS_PAGE;
        read.extend(page);
        if !full {
            return Ok(read);
        }
    }
}

/// The observations under `observer` whose directory and files were all
/// last written at or before `cutoff`; the cursor and other files are not
/// observations.
fn expired_observations(
    files: &dyn RunFiles,
    observer: &Path,
    cutoff: i64,
) -> Result<Vec<PathBuf>> {
    let mut expired = Vec::new();
    for dir in entries(files, observer)? {
        let named = dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(observation_name);
        if !named || !real_dir(files, &dir) {
            continue;
        }
        // One whose files cannot be listed is kept; the others go on.
        let Ok(inside) = entries(files, &dir) else {
            continue;
        };
        let mut newest = unix(files.modified(&dir).ok());
        for file in inside {
            newest = newest.max(unix(files.modified(&file).ok()));
        }
        if newest <= cutoff {
            expired.push(dir);
        }
    }
    Ok(expired)
}

/// The copies in `backups` past the newest [`RetentionConfig::backups`].
fn old_backups(
    files: &dyn RunFiles,
    backups: &Path,
    config: &RetentionConfig,
) -> Result<Vec<PathBuf>> {
    let copies: Vec<PathBuf> = entries(files, backups)?
        .into_iter()
        .filter(|path| matches!(files.entry_kind(path), Ok(EntryKind::File)))
        .collect();
    let names: Vec<&str> = copies
        .iter()
        .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
        .collect();
    Ok(config
        .backups_to_remove(&names)
        .into_iter()
        .map(|name| backups.join(name))
        .collect())
}

/// Whether `path` is a directory itself, not a link to one.
fn real_dir(files: &dyn RunFiles, path: &Path) -> bool {
    matches!(files.entry_kind(path), Ok(EntryKind::Dir))
}

/// `at` in UNIX seconds; a time that cannot be read is the latest, so it
/// keeps what it was read of.
fn unix(at: Option<SystemTime>) -> i64 {
    at.and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(i64::MAX)
}
