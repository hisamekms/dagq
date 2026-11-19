//! Run files in memory for the use cases' unit tests.

use anyhow::Result;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::RunFiles;

/// Run files in memory, each with the time it was written.
#[derive(Default)]
pub struct MemoryFiles {
    files: Mutex<HashMap<PathBuf, (SystemTime, Vec<u8>)>>,
    /// The most bytes one read takes, as the run directory's reads are
    /// bounded; none when unbounded.
    read_limit: Option<u64>,
    busy_locks: Mutex<std::collections::HashSet<PathBuf>>,
    busy_next_shared_locks: Mutex<std::collections::HashSet<PathBuf>>,
    observed_locks: Mutex<HashMap<PathBuf, Arc<ExclusiveLockProbe>>>,
    rejected_suffix: Mutex<Option<String>>,
    /// The directories that are there ([`Self::put_dir`]); none else is.
    dirs: Mutex<std::collections::HashSet<PathBuf>>,
    /// Injected directory measurements; unspecified paths are unmeasured.
    tree_sizes: Mutex<HashMap<PathBuf, u64>>,
    removed_dir_hooks: Mutex<HashMap<PathBuf, Box<dyn FnOnce() + Send>>>,
    /// The milliseconds the files' `now` moved on since its start, one at
    /// each write, when the files tick ([`Self::ticking`]); `None` keeps
    /// one `now`.
    ticks: Option<std::sync::atomic::AtomicU64>,
}

/// An opt-in in-memory flock model. A failed probe records contention
/// and dropping a guard releases its share of the lock.
#[derive(Default)]
pub(crate) struct ExclusiveLockProbe {
    holders: AtomicUsize,
    blocked: AtomicUsize,
}

impl ExclusiveLockProbe {
    pub(crate) fn blocked_attempts(&self) -> usize {
        self.blocked.load(Ordering::SeqCst)
    }
}

const EXCLUSIVE: usize = usize::MAX;

struct ObservedLock {
    probe: Arc<ExclusiveLockProbe>,
    exclusive: bool,
}

impl Drop for ObservedLock {
    fn drop(&mut self) {
        if self.exclusive {
            self.probe.holders.store(0, Ordering::SeqCst);
        } else {
            self.probe.holders.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl MemoryFiles {
    /// Model a compiler lock held by another process until the test ends.
    pub fn hold_lock(&self, path: &Path) {
        self.busy_locks.lock().unwrap().insert(path.to_owned());
    }

    /// Simulate an exclusive holder arriving just before the next shared lock.
    pub fn hold_next_shared_lock(&self, path: &Path) {
        self.busy_next_shared_locks
            .lock()
            .unwrap()
            .insert(path.to_owned());
    }

    pub(crate) fn observe_exclusive_lock(&self, path: &Path) -> Arc<ExclusiveLockProbe> {
        self.observed_locks
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .clone()
    }

    fn observed_lock(&self, path: &Path, exclusive: bool) -> Option<Box<dyn std::any::Any + Send>> {
        let Some(probe) = self.observed_locks.lock().unwrap().get(path).cloned() else {
            return Some(Box::new(()));
        };
        let acquired = probe
            .holders
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |holders| {
                if exclusive {
                    (holders == 0).then_some(EXCLUSIVE)
                } else {
                    holders.checked_add(1)
                }
            });
        if acquired.is_err() {
            probe.blocked.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        Some(Box::new(ObservedLock { probe, exclusive }))
    }

    /// Files whose reads take at most `limit` bytes each.
    pub fn bounded(limit: u64) -> Self {
        Self {
            read_limit: Some(limit),
            ..Self::default()
        }
    }

    /// Files whose `now` moves on a millisecond at each write, so that a
    /// file written after another is newer than it, as a file system's
    /// are: what a session's wrapper and the supervisor compare (a resumed
    /// turn's receipt newer than its resume's start, an idle marker newer
    /// than the receipt) when they run on these files together.
    pub fn ticking() -> Self {
        Self {
            ticks: Some(std::sync::atomic::AtomicU64::new(0)),
            ..Self::default()
        }
    }

    /// Inject a write failure only for the named file suffix.
    pub fn reject_writes_ending_with(&self, suffix: &str) {
        *self.rejected_suffix.lock().unwrap() = Some(suffix.to_owned());
    }

    fn check_limit(&self, len: u64) -> io::Result<()> {
        match self.read_limit {
            Some(limit) if len > limit => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exceeds the read limit",
            )),
            _ => Ok(()),
        }
    }

    /// The bytes of `path`, whatever the read limit.
    pub fn bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        let files = self.files.lock().unwrap();
        files
            .get(path)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(missing)
    }

    /// Make `dir` a directory that is there.
    pub fn put_dir(&self, dir: &Path) {
        self.dirs.lock().unwrap().insert(dir.to_owned());
    }

    /// Supply a directory measurement without allocating its contents.
    pub fn set_tree_size(&self, dir: &Path, bytes: u64) {
        self.tree_sizes
            .lock()
            .unwrap()
            .insert(dir.to_owned(), bytes);
    }

    /// Run a hook after the next recursive removal of this directory.
    pub fn after_remove_dir_all(&self, dir: &Path, hook: impl FnOnce() + Send + 'static) {
        self.removed_dir_hooks
            .lock()
            .unwrap()
            .insert(dir.to_owned(), Box::new(hook));
    }

    pub fn put(&self, path: &Path, modified: SystemTime, contents: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (modified, contents.as_bytes().to_vec()));
    }
}

/// The files' `now` before any tick, after the Unix epoch.
const START: Duration = Duration::from_secs(1_000_000);

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl RunFiles for MemoryFiles {
    fn try_lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        if self.busy_locks.lock().unwrap().contains(path) {
            return Ok(None);
        }
        Ok(self.observed_lock(path, true))
    }

    fn try_lock_shared(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        if self.busy_next_shared_locks.lock().unwrap().remove(path) {
            return Ok(None);
        }
        Ok(self.observed_lock(path, false))
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn std::any::Any + Send>> {
        // The observed model reports contention instead of waiting on
        // the test thread that has to release the competing guard.
        self.observed_lock(path, true)
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        for parent in dir.ancestors() {
            self.put_dir(parent);
        }
        Ok(())
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        self.put_dir(dir);
        Ok(())
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if self
            .rejected_suffix
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|suffix| path.to_string_lossy().ends_with(suffix))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected write failure",
            ));
        }
        let stamp = match &self.ticks {
            // The tick this write took, so that two writes at once each
            // take one of their own.
            Some(ticks) => {
                let tick = ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                UNIX_EPOCH + START + Duration::from_millis(tick)
            }
            None => self.now(),
        };
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (stamp, contents.to_vec()));
        Ok(())
    }
    fn copy(&self, _: &Path, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = self.bytes(path)?;
        self.check_limit(bytes.len() as u64)?;
        Ok(bytes)
    }
    fn read_range(&self, path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.check_limit(len)?;
        let bytes = self.bytes(path)?;
        let from = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let to = from.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
        Ok(bytes[from..to.min(bytes.len())].to_vec())
    }
    /// Unbounded, as the run directory's tail reads are.
    fn read_tail(&self, path: &Path, len: u64) -> io::Result<Vec<u8>> {
        let bytes = self.bytes(path)?;
        let from = bytes
            .len()
            .saturating_sub(usize::try_from(len).unwrap_or(usize::MAX));
        Ok(bytes[from..].to_vec())
    }
    fn size(&self, path: &Path) -> io::Result<u64> {
        self.bytes(path).map(|bytes| bytes.len() as u64)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        let files = self.files.lock().unwrap();
        files.get(path).map(|(at, _)| *at).ok_or_else(missing)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        Ok(self.files.lock().unwrap().get(path).cloned())
    }
    fn is_file(&self, path: &Path) -> bool {
        self.exists(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.dirs.lock().unwrap().contains(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        Ok(files
            .keys()
            .chain(self.dirs.lock().unwrap().iter())
            .filter(|path| path.parent() == Some(dir))
            .cloned()
            .collect())
    }
    /// A file, or a directory with every file and directory under it.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        if let Some(file) = files.remove(from) {
            files.insert(to.to_owned(), file);
            return Ok(());
        }
        let under: Vec<PathBuf> = files
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        let mut dirs = self.dirs.lock().unwrap();
        let dirs_under: Vec<PathBuf> = dirs
            .iter()
            .filter(|dir| dir.starts_with(from))
            .cloned()
            .collect();
        if under.is_empty() && dirs_under.is_empty() {
            return Err(missing());
        }
        let moved = |path: &Path| to.join(path.strip_prefix(from).expect("a path under from"));
        for path in under {
            let file = files.remove(&path).expect("a file just listed");
            files.insert(moved(&path), file);
        }
        for dir in dirs_under {
            dirs.remove(&dir);
            dirs.insert(moved(&dir));
        }
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .remove(path)
            .map(drop)
            .ok_or_else(missing)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        Ok(self.tree_sizes.lock().unwrap().get(dir).copied())
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .retain(|path, _| !path.starts_with(dir));
        self.dirs.lock().unwrap().retain(|at| !at.starts_with(dir));
        self.tree_sizes
            .lock()
            .unwrap()
            .retain(|at, _| !at.starts_with(dir));
        let hook = self.removed_dir_hooks.lock().unwrap().remove(dir);
        if let Some(hook) = hook {
            hook();
        }
        Ok(())
    }
    /// A directory with no file under it.
    fn remove_empty_dir(&self, dir: &Path) -> io::Result<bool> {
        let empty = !self
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|path| path.starts_with(dir));
        let removed = empty && self.dirs.lock().unwrap().remove(dir);
        if removed {
            self.tree_sizes.lock().unwrap().remove(dir);
        }
        Ok(removed)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        let mut bytes = match self.bytes(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.write(path, &bytes)
    }
    /// A directory or file put is its own resolved path; no link is
    /// followed, as none is put.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.is_dir(path) || self.exists(path) {
            Ok(path.to_owned())
        } else {
            Err(missing())
        }
    }
    /// `text`, then `body` (empty when there is none) fenced with one
    /// backtick more than its longest run, three at least.
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        let body = self.read_to_string(body).unwrap_or_default();
        let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat((longest + 1).max(3));
        let newline = if body.is_empty() || body.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        self.write(
            path,
            format!("{text}{fence}{info}\n{body}{newline}{fence}\n").as_bytes(),
        )?;
        Ok(())
    }
    fn now(&self) -> SystemTime {
        let ticks = self
            .ticks
            .as_ref()
            .map_or(0, |ticks| ticks.load(std::sync::atomic::Ordering::SeqCst));
        UNIX_EPOCH + START + Duration::from_millis(ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_preserves_lines_and_honors_write_failure() {
        let files = MemoryFiles::default();
        let path = Path::new("rebind.jsonl");
        files.append_line(path, "first").unwrap();
        files.append_line(path, "second").unwrap();
        assert_eq!(files.bytes(path).unwrap(), b"first\nsecond\n");
        files.reject_writes_ending_with("rebind.jsonl");
        assert_eq!(
            files.append_line(path, "rejected").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(files.bytes(path).unwrap(), b"first\nsecond\n");
    }
}
