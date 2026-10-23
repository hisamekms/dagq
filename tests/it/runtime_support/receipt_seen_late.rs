//! Run files that show a worker's receipt or its idle marker late, which
//! the headless worker's and the handoff's runtime tests share.
use super::*;
use dagq::application::RunFiles;
use std::io;

/// Run files that hide a receipt from the first look at it made once the
/// turn that wrote it has ended (its idle marker is not older), and the
/// idle marker until that look: the supervisor's pass looked for the
/// receipt just before the turn wrote it and ended, and reads the idle
/// after it, as a pass slowed by load does. Hiding the marker too keeps
/// the end of the turn from falling later in a pass, where the hidden look
/// would be the stall watch's own.
/// With `idle_unseen` it shows the receipt as it is and never shows the
/// idle marker: the supervisor sees the session's end only as its
/// wrapper's exit.
#[derive(Default)]
pub struct ReceiptSeenLate {
    hidden: Mutex<bool>,
    idle_unseen: bool,
}

impl ReceiptSeenLate {
    /// Run files that never show the idle marker (`idle_unseen`).
    pub fn idle_unseen() -> Self {
        Self {
            idle_unseen: true,
            ..Self::default()
        }
    }

    /// Whether `path` is the idle marker while it is hidden.
    fn idle_hidden(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == "idle.json")
            && (self.idle_unseen || !*self.hidden.lock().unwrap())
    }

    fn turn_ended(receipt: &Path) -> bool {
        let idle = receipt.with_file_name("idle.json");
        match (
            LocalRunFiles.modified(receipt),
            LocalRunFiles.modified(&idle),
        ) {
            (Ok(written), Ok(ended)) => ended >= written,
            _ => false,
        }
    }
}

impl RunFiles for ReceiptSeenLate {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if self.idle_hidden(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        LocalRunFiles.read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        if self.idle_hidden(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        LocalRunFiles.read_to_string(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        if self.idle_hidden(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        if self.idle_hidden(path) {
            return Ok(None);
        }
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        if self.idle_hidden(path) || !LocalRunFiles.is_file(path) {
            return false;
        }
        if !self.idle_unseen && path.file_name().is_some_and(|name| name == "receipt.json") {
            // Hidden while the turn runs, then from one more look.
            if !Self::turn_ended(path) {
                return false;
            }
            let mut hidden = self.hidden.lock().unwrap();
            if !*hidden {
                *hidden = true;
                return false;
            }
        }
        true
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn exists(&self, path: &Path) -> bool {
        !self.idle_hidden(path) && LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.remove_dir_all(dir)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}
