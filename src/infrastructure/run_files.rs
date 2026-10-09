//! The run directory on the local file system ([`RunFiles`]).

use std::{
    collections::HashSet,
    fs,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};

use super::agent_dir::{self, Directory};
use crate::application::{EntryKind, RunFiles};

/// The directories Claude Code keeps its sessions' scratchpads under on
/// this host (task 1100): `claude-<uid>` in the system's `/tmp` (where it
/// puts them, not in `$TMPDIR`), in `$TMPDIR` and in `$CLAUDE_CODE_TMPDIR`
/// when set, each with its links resolved, those that exist, without
/// repeats. The supervisor removes the scratchpad of an ended run under
/// each.
pub fn claude_scratchpad_roots() -> Vec<PathBuf> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let name = format!("claude-{}", unsafe { libc::getuid() });
    let mut bases = vec![PathBuf::from("/tmp")];
    bases.extend(
        ["TMPDIR", "CLAUDE_CODE_TMPDIR"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from),
    );
    let mut roots: Vec<PathBuf> = Vec::new();
    for base in bases {
        let Ok(root) = fs::canonicalize(base.join(&name)) else {
            continue;
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// The run files as the local file system holds them. A path at or
/// below a run directory (`agent_dir::in_run_dir`: the worker writes
/// there) goes through `agent_dir`'s descriptors without following links,
/// reads only regular files up to `agent_dir::FILE_BYTES` and replaces a
/// file instead of writing through it. Every other path is the host's
/// (the queue's dir and DB, installed binaries, scratchpads) and is used
/// as `std::fs` does, links followed and without the limit.
pub struct LocalRunFiles;

use agent_dir::in_run_dir;

/// Open `path` to read it: [`agent_dir::read_file`] in a run directory,
/// following links elsewhere.
fn open_to_read(path: &Path) -> io::Result<fs::File> {
    if in_run_dir(path) {
        agent_dir::read_file(path)
    } else {
        fs::File::open(path)
    }
}

impl RunFiles for LocalRunFiles {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        if !in_run_dir(dir) {
            return fs::create_dir_all(dir);
        }
        match Directory::open(dir) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = dir.parent().ok_or(error)?;
                self.create_dir_all(parent)?;
                match self.create_new_dir(dir) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        Directory::open(dir).map(drop)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        if !in_run_dir(dir) {
            return fs::create_dir(dir);
        }
        let (parent, name) = Directory::parent(dir)?;
        parent.mkdir(name)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        if !in_run_dir(path) {
            return fs::write(path, contents);
        }
        let (dir, name) = Directory::parent(path)?;
        dir.write(name, contents).map(drop)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        if !in_run_dir(to) {
            return fs::copy(from, to).map(drop);
        }
        // The source is the runtime's own (its binary, which may be
        // reached through a link) and can exceed the run-material read
        // limit. Keep its mode and stream a snapshot of its size into a
        // new file of the run directory.
        let source = fs::File::open(from)?;
        let metadata = source.metadata()?;
        let (dir, name) = Directory::parent(to)?;
        dir.replace(name, |target| {
            io::copy(&mut source.take(metadata.len()), target)?;
            target.set_permissions(metadata.permissions())
        })
        .map(drop)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if !in_run_dir(path) {
            return fs::read(path);
        }
        agent_dir::read_bounded(agent_dir::read_file(path)?)
    }
    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Vec<u8>> {
        let mut file = open_to_read(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        if !in_run_dir(path) {
            file.read_to_end(&mut bytes)?;
            return Ok(bytes);
        }
        file.take(agent_dir::FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > agent_dir::FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "turn output exceeds 64 MiB per read",
            ));
        }
        Ok(bytes)
    }
    fn read_range(&self, path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        if in_run_dir(path) && len > agent_dir::FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a read of the run directory takes at most 64 MiB",
            ));
        }
        let mut file = open_to_read(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(len).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    fn read_tail(&self, path: &Path, bytes: u64) -> io::Result<Vec<u8>> {
        let mut file = open_to_read(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(bytes)))?;
        let mut tail = Vec::new();
        file.take(bytes).read_to_end(&mut tail)?;
        Ok(tail)
    }
    fn size(&self, path: &Path) -> io::Result<u64> {
        Ok(open_to_read(path)?.metadata()?.len())
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        if !in_run_dir(path) {
            return fs::metadata(path)?.modified();
        }
        agent_dir::read_file(path)?.metadata()?.modified()
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        let file = match open_to_read(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error)
                if in_run_dir(path)
                    && (matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
                        || error.kind() == io::ErrorKind::InvalidInput) =>
            {
                tracing::warn!(path = %path.display(), %error, "ignore unsafe idle marker");
                return Ok(None);
            }
            Err(error) => return Err(error).context("inspect idle marker"),
        };
        let modified = file.metadata()?.modified()?;
        let bytes = if in_run_dir(path) {
            agent_dir::read_bounded(file)
        } else {
            let mut bytes = Vec::new();
            (&file).read_to_end(&mut bytes).map(|_| bytes)
        }
        .context("read idle marker")?;
        Ok(Some((modified, bytes)))
    }
    fn is_file(&self, path: &Path) -> bool {
        if !in_run_dir(path) {
            return path.is_file();
        }
        match Directory::parent(path).and_then(|(dir, name)| dir.kind(name)) {
            Ok(Some(EntryKind::File)) => true,
            Ok(Some(kind)) => {
                tracing::warn!(path = %path.display(), ?kind, "ignore non-regular run file");
                false
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), %error, "cannot inspect run file");
                false
            }
            _ => false,
        }
    }
    fn is_dir(&self, path: &Path) -> bool {
        if !in_run_dir(path) {
            return path.is_dir();
        }
        Directory::open(path).is_ok()
    }
    fn exists(&self, path: &Path) -> bool {
        if !in_run_dir(path) {
            return path.exists();
        }
        self.is_dir(path) || self.is_file(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        if !in_run_dir(dir) {
            return Ok(fs::read_dir(dir)?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .collect());
        }
        Ok(Directory::open(dir)?
            .names()?
            .into_iter()
            .map(|name| dir.join(name))
            .collect())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if !in_run_dir(from) && !in_run_dir(to) {
            return fs::rename(from, to);
        }
        let (from_dir, from) = Directory::parent(from)?;
        let (to_dir, to) = Directory::parent(to)?;
        from_dir.rename(from, &to_dir, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if !in_run_dir(path) {
            return fs::remove_file(path);
        }
        let (dir, name) = Directory::parent(path)?;
        dir.remove(name)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        // A link in place of the root is not counted, wherever it is.
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let root = match Directory::open(dir) {
            Ok(root) => root,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut seen = HashSet::new();
        let mut bytes = 0;
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            let stat = dir.stat(None)?;
            if seen.insert((stat.st_dev, stat.st_ino)) {
                bytes += stat.st_blocks as u64 * 512;
            }
            for name in dir.names()? {
                let stat = match dir.stat(Some(&name)) {
                    Ok(stat) => stat,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    match dir.child(&name) {
                        Ok(child) => pending.push(child),
                        Err(error)
                            if error.kind() == io::ErrorKind::NotFound
                                || matches!(
                                    error.raw_os_error(),
                                    Some(libc::ELOOP | libc::ENOTDIR)
                                ) => {}
                        Err(error) => return Err(error),
                    }
                } else if seen.insert((stat.st_dev, stat.st_ino)) {
                    bytes += stat.st_blocks as u64 * 512;
                }
            }
        }
        Ok(Some(bytes))
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        if !in_run_dir(path) {
            return fs::remove_dir_all(path);
        }
        let (dir, name) = Directory::parent(path)?;
        if matches!(dir.kind(name)?, Some(EntryKind::File | EntryKind::Other)) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        dir.remove_tree(name)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        path.canonicalize()
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        let mut source = open_to_read(body).with_context(|| format!("open {}", body.display()))?;
        let size = source.metadata()?.len();
        if in_run_dir(body) && size > agent_dir::FILE_BYTES {
            anyhow::bail!("run file exceeds 64 MiB");
        }
        let (longest, last) = backtick_run_and_last_byte((&mut source).take(size))?;
        source.seek(SeekFrom::Start(0))?;
        let fence = "`".repeat(longest.max(2) + 1);
        let fenced = |target: &mut fs::File| {
            let mut out = BufWriter::new(target);
            writeln!(out, "{text}{fence}{info}")?;
            io::copy(&mut source.take(size), &mut out)?;
            if last.is_some_and(|byte| byte != b'\n') {
                out.write_all(b"\n")?;
            }
            writeln!(out, "{fence}")?;
            out.into_inner()
                .map_err(|error| error.into_error())?
                .sync_all()
        };
        if in_run_dir(path) {
            let (dir, name) = Directory::parent(path)?;
            dir.replace(name, fenced)?;
        } else {
            let mut target =
                fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
            fenced(&mut target).with_context(|| format!("write {}", path.display()))?;
        }
        Ok(())
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn try_lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        use std::os::fd::AsRawFd;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // SAFETY: flock on a descriptor this function owns; the lock goes
        // with the file when the guard drops or the process ends.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Box::new(file)));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(error)
        }
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn std::any::Any + Send>> {
        use std::os::fd::AsRawFd;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        // SAFETY: flock on a descriptor this function owns; it blocks until
        // the lock is free, and the lock goes with the file when the guard
        // drops or the process ends.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(Box::new(file));
        }
        Err(io::Error::last_os_error())
    }
}

/// Scan the bounded snapshot through the same file descriptor used for
/// the subsequent copy, with constant memory even for large diffs.
fn backtick_run_and_last_byte(mut source: impl Read) -> io::Result<(usize, Option<u8>)> {
    let mut buffer = [0u8; 64 * 1024];
    let (mut longest, mut run, mut last) = (0, 0, None);
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            return Ok((longest, last));
        }
        for &byte in &buffer[..read] {
            run = if byte == b'`' { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        last = Some(buffer[read - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_scratchpad_roots_are_existing_claude_directories_without_repeats() {
        let roots = claude_scratchpad_roots();
        let name = format!("claude-{}", unsafe { libc::getuid() });
        for (i, root) in roots.iter().enumerate() {
            assert!(root.is_absolute() && root.is_dir(), "{}", root.display());
            assert_eq!(root.file_name().unwrap().to_str(), Some(name.as_str()));
            assert!(!roots[..i].contains(root));
        }
    }

    #[test]
    fn a_lock_is_held_until_its_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalRunFiles;
        let path = dir.path().join("lock");
        let guard = files.try_lock(&path).unwrap().expect("a free lock");
        assert!(files.try_lock(&path).unwrap().is_none());
        drop(guard);
        assert!(files.try_lock(&path).unwrap().is_some());
        assert!(files.try_lock(&dir.path().join("none/lock")).is_err());
    }

    /// A waiting lock is taken only once the holder's guard drops, and
    /// keeps a lock taken without waiting out while it is held.
    #[test]
    fn a_waiting_lock_is_taken_once_the_holder_drops_its_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = LocalRunFiles.try_lock(&path).unwrap().expect("a free lock");
        let (sender, taken) = std::sync::mpsc::channel();
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let guard = LocalRunFiles.lock(&path).unwrap();
                sender.send(()).unwrap();
                guard
            })
        };
        assert!(taken.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        taken.recv_timeout(Duration::from_secs(30)).unwrap();
        let guard = waiter.join().unwrap();
        assert!(LocalRunFiles.try_lock(&path).unwrap().is_none());
        drop(guard);
        assert!(LocalRunFiles.try_lock(&path).unwrap().is_some());
    }

    #[test]
    fn local_run_files_read_what_they_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let files = LocalRunFiles;
        let run_dir = dir.path().join("runs/a");
        files.create_dir_all(&dir.path().join("runs")).unwrap();
        files.create_new_dir(&run_dir).unwrap();
        assert!(files.create_new_dir(&run_dir).is_err());
        assert!(files.is_dir(&run_dir) && !files.is_file(&run_dir));
        let prompt = run_dir.join("prompt.txt");
        files.write(&prompt, b"hello").unwrap();
        files.copy(&prompt, &run_dir.join("copy.txt")).unwrap();
        assert_eq!(
            files.read_to_string(&run_dir.join("copy.txt")).unwrap(),
            "hello"
        );
        assert_eq!(files.read(&prompt).unwrap(), b"hello");
        assert!(files.exists(&prompt) && files.is_file(&prompt));
        let modified = files.modified(&prompt).unwrap();
        assert!(modified <= files.now() + Duration::from_secs(1));
        let (stamped, bytes) = files.read_stamped(&prompt).unwrap().unwrap();
        assert_eq!((stamped, bytes.as_slice()), (modified, b"hello".as_slice()));
        assert!(files.read_stamped(&run_dir.join("none")).unwrap().is_none());
        assert!(files.read_stamped(&run_dir).unwrap().is_none());
        assert!(files.modified(&run_dir.join("none")).is_err());
    }

    /// The runtime's own paths may be links: its binary copied into a run
    /// directory, a queue under a linked directory. A debug log larger
    /// than the whole-file limit still has its tail read.
    #[test]
    fn host_links_are_followed_and_a_large_log_has_its_tail_read() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let files = LocalRunFiles;
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        symlink(&data, dir.path().join("linked")).unwrap();
        let queue = dir.path().join("linked/queue");
        files.create_dir_all(&queue.join("runs/a")).unwrap();
        files.create_dir_all(&queue.join("host/samples")).unwrap();
        assert!(data.join("queue/host/samples").is_dir());
        let binary = data.join("dagq-real");
        fs::write(&binary, b"binary").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let runner = dir.path().join("dagq");
        symlink(&binary, &runner).unwrap();
        let copy = queue.join("runs/a/dagq");
        files.copy(&runner, &copy).unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"binary");
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let log = queue.join("runs/a/debug.log");
        let file = fs::File::create(&log).unwrap();
        file.set_len(agent_dir::FILE_BYTES + 10).unwrap();
        drop(file);
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"hook failed").unwrap();
        assert!(files.read(&log).is_err());
        let tail = files.read_tail(&log, 11).unwrap();
        assert_eq!(tail, b"hook failed");
        // A bounded range of the large log is read; one past the limit is not.
        let end = agent_dir::FILE_BYTES + 10;
        assert_eq!(files.read_range(&log, end, 4).unwrap(), b"hook");
        assert_eq!(files.read_range(&log, end + 5, 99).unwrap(), b"failed");
        assert_eq!(files.read_range(&log, end + 99, 4).unwrap(), b"");
        let error = files
            .read_range(&log, 0, agent_dir::FILE_BYTES + 1)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(files.read_tail(&copy, 1024).unwrap(), b"binary");
    }

    /// A host file reached through a link (an installed binary, a queue
    /// DB) is a file to the runtime, read and written
    /// through the link as `std::fs` does; in a run directory the same
    /// link is not a file, is not read, and is replaced rather than
    /// written through.
    #[test]
    fn a_linked_host_file_is_a_file_and_a_linked_run_file_is_not() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let files = LocalRunFiles;
        let target = dir.path().join("dagq-real");
        fs::write(&target, b"binary").unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let linked = bin.join("dagq");
        symlink(&target, &linked).unwrap();
        assert!(files.is_file(&linked) && files.exists(&linked));
        assert_eq!(files.read(&linked).unwrap(), b"binary");
        assert_eq!(files.read_to_string(&linked).unwrap(), "binary");
        assert_eq!(files.read_tail(&linked, 3).unwrap(), b"ary");
        assert_eq!(files.read_range(&linked, 1, 3).unwrap(), b"ina");
        assert!(files.modified(&linked).is_ok());
        assert!(files.read_stamped(&linked).unwrap().is_some());
        files.write(&linked, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(fs::symlink_metadata(&linked).unwrap().is_symlink());

        let run = dir.path().join("queue/runs/a");
        files.create_dir_all(&run).unwrap();
        let marker = run.join("idle.json");
        symlink(&target, &marker).unwrap();
        assert!(!files.is_file(&marker) && !files.exists(&marker));
        assert!(files.read(&marker).is_err());
        assert!(files.read_stamped(&marker).unwrap().is_none());
        files.write(&marker, b"safe").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!fs::symlink_metadata(&marker).unwrap().is_symlink());
    }
}
