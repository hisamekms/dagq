//! Run files in memory for the use cases' unit tests.

use anyhow::Result;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Mutex,
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
    /// The directories that are there ([`Self::put_dir`]); none else is.
    dirs: Mutex<std::collections::HashSet<PathBuf>>,
    /// The milliseconds the files' `now` moved on since its start, one at
    /// each write, when the files tick ([`Self::ticking`]); `None` keeps
    /// one `now`.
    ticks: Option<std::sync::atomic::AtomicU64>,
}

impl MemoryFiles {
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
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        self.put_dir(dir);
        Ok(())
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        let stamp = match &self.ticks {
            // The tick this write took, so that two writes at once each
            // take one of their own.
            Some(ticks) => {
                let tick = ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                UNIX_EPOCH + START + Duration::from_millis(tick)
            }
            None => self.now(),
        };
        self.put(path, stamp, &String::from_utf8_lossy(contents));
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
    fn tree_size(&self, _: &Path) -> io::Result<Option<u64>> {
        Ok(None)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .retain(|path, _| !path.starts_with(dir));
        self.dirs.lock().unwrap().retain(|at| !at.starts_with(dir));
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
        Ok(empty && self.dirs.lock().unwrap().remove(dir))
    }
    fn append_line(&self, _: &Path, _: &str) -> io::Result<()> {
        unimplemented!("no test appends to a run file")
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
