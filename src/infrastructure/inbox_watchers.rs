//! The records of the inbox's watches (ADR-t906-1): one JSON file per
//! `watch --role inbox` in `inbox-watchers/` of the queue's directory (where
//! `dagq locate`'s `db` is), never the queue DB, which `watch` only reads.
//! A file is named by its watch's start in milliseconds and pid, so a pid
//! the system reuses for a later watch gets a file of its own; a watch
//! rewrites only its own file, atomically (a temporary file and a rename).
//! The judgment on the records is [`crate::application::inbox_watcher`].

use crate::application::inbox_watcher::WatcherRecord;
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub use crate::application::inbox_watcher::{DIR, dir};

/// A record whose watch was last seen this long ago is deleted when the
/// next watch starts.
pub const PRUNE_AFTER_SECS: i64 = 7 * 24 * 3600;

/// Every record in `dir`, skipping files that cannot be read or parsed (a
/// missing directory is no record).
pub fn read(dir: &Path) -> Vec<WatcherRecord> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| fs::read(&path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect()
}

/// The record of this process's watch, written at its start, rewritten at
/// every heartbeat and at its end. Dropped without [`Self::end`] (the
/// watch failed), it writes its end on the system clock.
#[derive(Debug)]
pub struct WatcherFile {
    path: PathBuf,
    record: WatcherRecord,
}

impl WatcherFile {
    /// Writes the record of a watch started at `now` (unix seconds, with
    /// `started_ms` naming the file; `timeout_secs` is `None` for a watch
    /// without a timeout) and deletes the records last seen more than
    /// [`PRUNE_AFTER_SECS`] before `now`.
    pub fn start(
        dir: &Path,
        pid: u32,
        now: i64,
        started_ms: i64,
        timeout_secs: Option<i64>,
        interval_secs: i64,
    ) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        prune(dir, now);
        let file = Self {
            path: dir.join(format!("{started_ms}-{pid}.json")),
            record: WatcherRecord {
                pid,
                started_at: now,
                heartbeat_at: now,
                ended_at: None,
                timeout_secs,
                interval_secs,
            },
        };
        file.write()?;
        Ok(file)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Renews the heartbeat to `now`.
    pub fn heartbeat(&mut self, now: i64) -> Result<()> {
        self.record.heartbeat_at = now;
        self.write()
    }

    /// Writes the watch's end at `now`.
    pub fn end(&mut self, now: i64) -> Result<()> {
        self.record.heartbeat_at = self.record.heartbeat_at.max(now);
        self.record.ended_at = Some(now);
        self.write()
    }

    fn write(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(&self.record)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| format!("rename to {}", self.path.display()))
    }
}

impl Drop for WatcherFile {
    fn drop(&mut self) {
        if self.record.ended_at.is_none() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let _ = self.end(now);
        }
    }
}

/// Deletes the records in `dir` last seen more than [`PRUNE_AFTER_SECS`]
/// before `now`.
fn prune(dir: &Path, now: i64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let old = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WatcherRecord>(&bytes).ok())
            .is_some_and(|record| now - record.last_seen_at() > PRUNE_AFTER_SECS);
        if old {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_writes_its_start_heartbeat_and_end_and_prunes_old_records() {
        let root = tempfile::tempdir().unwrap();
        let dir = dir(&root.path().join("queue.db"));
        assert_eq!(dir, root.path().join(DIR));
        assert!(read(&dir).is_empty());

        let now = 10 * PRUNE_AFTER_SECS;
        let mut old = WatcherFile::start(&dir, 7, 1_000, 1_000_000, Some(600), 2).unwrap();
        old.end(1_001).unwrap();
        let mut file = WatcherFile::start(&dir, 7, now, now * 1000, Some(600), 2).unwrap();
        // The old record is gone; a pid reused later has its own file.
        assert!(!old.path().exists());
        assert_ne!(old.path(), file.path());
        fs::write(dir.join("garbage.json"), "not json").unwrap();
        let records = read(&dir);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].started_at, now);
        assert_eq!(records[0].ended_at, None);

        file.heartbeat(now + 2).unwrap();
        assert_eq!(read(&dir)[0].heartbeat_at, now + 2);
        file.end(now + 3).unwrap();
        let record = &read(&dir)[0];
        assert_eq!(record.ended_at, Some(now + 3));
        assert_eq!(record.heartbeat_at, now + 3);
        assert_eq!(record.pid, 7);
        assert_eq!((record.timeout_secs, record.interval_secs), (Some(600), 2));
    }

    #[test]
    fn a_dropped_watch_writes_its_end() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DIR);
        drop(WatcherFile::start(&dir, 9, 100, 100_000, None, 2).unwrap());
        assert!(read(&dir)[0].ended_at.is_some());
        assert_eq!(read(&dir)[0].timeout_secs, None);
        assert_eq!(super::dir(Path::new("queue.db")), PathBuf::from(DIR));
    }
}
