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
}

impl MemoryFiles {
    /// Files whose reads take at most `limit` bytes each.
    pub fn bounded(limit: u64) -> Self {
        Self {
            read_limit: Some(limit),
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

    pub fn put(&self, path: &Path, modified: SystemTime, contents: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (modified, contents.as_bytes().to_vec()));
    }
}

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl RunFiles for MemoryFiles {
    fn create_dir_all(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn create_new_dir(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.put(path, self.now(), &String::from_utf8_lossy(contents));
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
    fn is_dir(&self, _: &Path) -> bool {
        false
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
    fn rename(&self, _: &Path, _: &Path) -> io::Result<()> {
        unimplemented!("no test renames a run file")
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
    fn remove_dir_all(&self, _: &Path) -> io::Result<()> {
        unimplemented!("no test removes a run directory")
    }
    fn append_line(&self, _: &Path, _: &str) -> io::Result<()> {
        unimplemented!("no test appends to a run file")
    }
    fn canonicalize(&self, _: &Path) -> io::Result<PathBuf> {
        unimplemented!("no test resolves a run file")
    }
    fn write_fenced(&self, _: &Path, _: &str, _: &str, _: &Path) -> Result<()> {
        unimplemented!("no test writes a review")
    }
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000)
    }
}
