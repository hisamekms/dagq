//! Directories an agent writes, opened without following links:
//! [`AgentDirHandle`] for ask requests, [`Directory`] for the supervisor's
//! and wrapper's other run material (ADR-t813-3 decisions 3 and 6).
//! The supervisor runs outside the worker's sandbox, so it must not do
//! for the worker what the sandbox stops. The run directory and its
//! children are worker-writable: the run directory is opened with
//! `O_NOFOLLOW`, its children relative to it, and entries are inspected,
//! read, renamed and removed through descriptors (`fstatat`, `openat`,
//! `renameat`, `unlinkat`). A replaced path never redirects those calls.
//! The queue's `runs/` and ancestors belong to the runtime.

use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::Path,
};

use crate::application::{AgentDir, AgentDirHandle, EntryKind};

/// How a directory is opened: never through a link, nor held up by one
/// that is something else (`O_NONBLOCK`).
const DIRECTORY: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// Open `dir` as [`crate::application::RunFiles::open_agent_dir`] says.
pub fn open(dir: &Path) -> io::Result<AgentDir> {
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent", dir.display()),
        ));
    };
    let parent_path = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `parent_path` is a NUL-terminated string that outlives the
    // call.
    let parent_fd = unsafe { libc::open(parent_path.as_ptr(), DIRECTORY) };
    if parent_fd < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(AgentDir::Missing),
            Some(libc::ELOOP | libc::ENOTDIR) => match std::fs::symlink_metadata(parent) {
                Ok(metadata) => match kind_of(metadata.file_type()) {
                    EntryKind::Dir => Err(error),
                    kind => Ok(AgentDir::ParentNot(kind)),
                },
                Err(gone) if gone.kind() == io::ErrorKind::NotFound => Ok(AgentDir::Missing),
                Err(other) => Err(other),
            },
            _ => Err(error),
        };
    }
    // SAFETY: `parent_fd` was just opened here and is owned by nothing else.
    let parent = Handle(unsafe { OwnedFd::from_raw_fd(parent_fd) });
    let name = name.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not UTF-8", dir.display()),
        )
    })?;
    let entry = entry_name(name)?;
    // SAFETY: the parent's descriptor is open and `entry` is NUL-terminated.
    let fd = unsafe { libc::openat(parent.fd(), entry.as_ptr(), DIRECTORY) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(AgentDir::Missing),
            // A link (`O_NOFOLLOW`) or no directory (`O_DIRECTORY`): say
            // what, from its own metadata. One that became a directory
            // meanwhile is looked at again at the next pass.
            Some(libc::ELOOP | libc::ENOTDIR) => match parent.kind(name)? {
                None => Ok(AgentDir::Missing),
                Some(EntryKind::Dir) => Err(error),
                Some(kind) => Ok(AgentDir::Not(kind)),
            },
            _ => Err(error),
        };
    }
    // SAFETY: `fd` was just opened here and is owned by nothing else.
    Ok(AgentDir::Open(Box::new(Handle(unsafe {
        OwnedFd::from_raw_fd(fd)
    }))))
}

fn kind_of(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Link
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::Other
    }
}

fn kind_of_mode(mode: libc::mode_t) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFREG => EntryKind::File,
        libc::S_IFDIR => EntryKind::Dir,
        libc::S_IFLNK => EntryKind::Link,
        _ => EntryKind::Other,
    }
}

/// `name` as one entry of the directory: no `/`, not `.` or `..`.
fn entry_name(name: impl AsRef<OsStr>) -> io::Result<CString> {
    let name = name.as_ref().as_bytes();
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not one directory entry",
        ));
    }
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct Handle(OwnedFd);

impl Handle {
    fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

fn names(fd: RawFd) -> io::Result<Vec<OsString>> {
    // A stream of its own over a duplicate (not inherited by children),
    // closed with it, read from the start.
    // SAFETY: the descriptor is open.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is an open directory descriptor that the
    // stream takes over.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the stream did not take it.
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    let mut names = Vec::new();
    // SAFETY: `stream` is an open directory stream until `closedir`, and
    // each entry is read before the next `readdir`.
    unsafe {
        libc::rewinddir(stream);
        loop {
            let entry = libc::readdir(stream);
            if entry.is_null() {
                break;
            }
            let name = CStr::from_ptr((*entry).d_name.as_ptr());
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(OsString::from_vec(name.to_bytes().to_owned()));
            }
        }
        libc::closedir(stream);
    }
    Ok(names)
}

impl AgentDirHandle for Handle {
    fn names(&self) -> io::Result<Vec<String>> {
        Ok(names(self.fd())?
            .into_iter()
            .filter_map(|name| name.into_string().ok())
            .collect())
    }

    fn kind(&self, name: &str) -> io::Result<Option<EntryKind>> {
        let name = entry_name(name)?;
        // SAFETY: an all-zero `stat` is a valid value to be written over.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor is open, `name` is NUL-terminated and
        // `stat` is a valid place to write.
        let result = unsafe {
            libc::fstatat(
                self.fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        match check(result) {
            Ok(()) => Ok(Some(kind_of_mode(stat.st_mode))),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn read(&self, name: &str, limit: usize) -> io::Result<Vec<u8>> {
        let name = entry_name(name)?;
        // `O_NONBLOCK`: a FIFO put there does not hold the open.
        // SAFETY: the descriptor is open and `name` is NUL-terminated.
        let fd = unsafe {
            libc::openat(
                self.fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened here and is owned by nothing else.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", name.to_string_lossy()),
            ));
        }
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("larger than {limit} bytes"),
            ));
        }
        Ok(bytes)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (entry_name(from)?, entry_name(to)?);
        let fd = self.fd();
        // SAFETY: the descriptor is open and both names are NUL-terminated.
        check(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) })
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let name = entry_name(name)?;
        // SAFETY: the descriptor is open and `name` is NUL-terminated.
        check(unsafe { libc::unlinkat(self.fd(), name.as_ptr(), 0) })
    }
}

/// A run directory (or its immediate child), pinned before any entry is
/// used. Its parent is opened without following links as well. The
/// queue's `runs/` and ancestors belong to the runtime.
pub(crate) struct Directory(Handle);

impl Directory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        // The host's directories (the queue's, `/tmp`, a linked data dir)
        // are the runtime's and may be links: follow them as `std::fs`
        // would. Only a run directory and what is in it are refused as
        // links, at that level and the one above.
        if !in_run_dir(path) {
            return Self::open_following(path);
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let parent = if in_run_dir(parent) {
            let parent = CString::new(parent.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: the path is NUL terminated; ownership is taken below.
            let fd = unsafe { libc::open(parent.as_ptr(), DIRECTORY) };
            check(fd)?;
            Self(Handle(unsafe { OwnedFd::from_raw_fd(fd) }))
        } else {
            Self::open_following(parent)?
        };
        parent.child(name)
    }

    fn open_following(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: the path is NUL terminated; ownership is taken below.
        let fd = unsafe { libc::open(path.as_ptr(), DIRECTORY & !libc::O_NOFOLLOW) };
        check(fd)?;
        Ok(Self(Handle(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    pub(crate) fn parent(path: &Path) -> io::Result<(Self, &OsStr)> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok((Self::open(parent)?, name))
    }

    pub(crate) fn names(&self) -> io::Result<Vec<OsString>> {
        names(self.0.fd())
    }
    pub(crate) fn kind(&self, name: impl AsRef<OsStr>) -> io::Result<Option<EntryKind>> {
        match self.stat(Some(name.as_ref())) {
            Ok(stat) => Ok(Some(kind_of_mode(stat.st_mode))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub(crate) fn remove(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = entry_name(name)?;
        // SAFETY: the descriptor is open and the name is NUL terminated.
        check(unsafe { libc::unlinkat(self.0.fd(), name.as_ptr(), 0) })
    }

    pub(crate) fn child(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = entry_name(name)?;
        // SAFETY: the descriptor is open and the name is NUL terminated.
        let fd = unsafe { libc::openat(self.0.fd(), name.as_ptr(), DIRECTORY) };
        check(fd)?;
        Ok(Self(Handle(unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    pub(crate) fn stat(&self, name: Option<&OsStr>) -> io::Result<libc::stat> {
        // SAFETY: zeroed stat is valid storage for fstat/fstatat.
        let mut stat = unsafe { std::mem::zeroed() };
        let result = match name {
            Some(name) => {
                let name = entry_name(name)?;
                // SAFETY: descriptor, name and output storage are valid.
                unsafe {
                    libc::fstatat(
                        self.0.fd(),
                        name.as_ptr(),
                        &mut stat,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                }
            }
            // SAFETY: descriptor and output storage are valid.
            None => unsafe { libc::fstat(self.0.fd(), &mut stat) },
        };
        check(result)?;
        Ok(stat)
    }

    pub(crate) fn remove_tree(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        if self.kind(name)? != Some(EntryKind::Dir) {
            return self.remove(name);
        }
        let child = self.child(name)?;
        for entry in child.names()? {
            child.remove_tree(&entry)?;
        }
        let name = entry_name(name)?;
        // SAFETY: unlinkat does not follow a replaced directory entry.
        check(unsafe { libc::unlinkat(self.0.fd(), name.as_ptr(), libc::AT_REMOVEDIR) })
    }

    pub(crate) fn mkdir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = entry_name(name)?;
        // SAFETY: descriptor and name are valid for the call.
        check(unsafe { libc::mkdirat(self.0.fd(), name.as_ptr(), 0o755) })
    }

    pub(crate) fn read_file(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
        let name = entry_name(name)?;
        // Nonblocking open, then fstat: a FIFO cannot hold the caller.
        // SAFETY: descriptor and name are valid for the call.
        let fd = unsafe {
            libc::openat(
                self.0.fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        check(fd)?;
        let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(file)
    }

    pub(crate) fn rename(
        &self,
        from: impl AsRef<OsStr>,
        to_dir: &Self,
        to: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        let (from, to) = (entry_name(from)?, entry_name(to)?);
        // SAFETY: both descriptors and names are valid for the call.
        check(unsafe { libc::renameat(self.0.fd(), from.as_ptr(), to_dir.0.fd(), to.as_ptr()) })
    }

    /// A fresh inode, never an existing link/hardlink/FIFO. Publish by
    /// rename relative to this same descriptor. Returning the open file
    /// also lets a child's stdout keep writing this inode after a worker
    /// swaps the path. O_EXCL makes even a guessed temporary name safe.
    pub(crate) fn write(&self, name: impl AsRef<OsStr>, bytes: &[u8]) -> io::Result<File> {
        self.replace(name, |file| file.write_all(bytes))
    }

    pub(crate) fn replace(
        &self,
        name: impl AsRef<OsStr>,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<File> {
        let name = name.as_ref();
        entry_name(name)?;
        let tmp = format!(".dagq-{}.tmp", uuid::Uuid::new_v4());
        let entry = entry_name(&tmp)?;
        // SAFETY: descriptor and name are valid; ownership is taken below.
        let fd = unsafe {
            libc::openat(
                self.0.fd(),
                entry.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK
                    | libc::O_CLOEXEC,
                0o600,
            )
        };
        check(fd)?;
        let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let result = write(&mut file).and_then(|()| self.rename(&tmp, self, name));
        if result.is_err() {
            let _ = self.remove(&tmp);
        }
        result?;
        Ok(file)
    }
}

/// Whether `path` is a run directory or below one: a component after the
/// first component named `runs` (the queue's `runs/`; the worker writes
/// from the run directory down). It is decided from the path alone: a
/// host path that happens to have a `runs` directory above the queue's
/// (`/Users/x/runs/project/...`) is treated as a run directory's too, which
/// only refuses links there, never follows more.
pub(crate) fn in_run_dir(path: &Path) -> bool {
    let mut components = path.components().map(|c| c.as_os_str());
    while let Some(component) = components.next() {
        if component == OsStr::new(super::location::RUNS_DIR_NAME) {
            return components.next().is_some();
        }
    }
    false
}

/// Open a regular file under its pinned parent. All whole-file reads of
/// run material are bounded, including a file that grows while read.
pub(crate) const FILE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) fn read_file(path: &Path) -> io::Result<File> {
    Directory::parent(path).and_then(|(dir, name)| dir.read_file(name)).inspect_err(|error| {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), %error, "cannot read regular run file without following links");
        }
    })
}
pub(crate) fn create_file(path: &Path) -> io::Result<File> {
    if !in_run_dir(path) {
        return File::create(path);
    }
    let (dir, name) = Directory::parent(path)?;
    dir.write(name, &[])
}
/// Diagnostic append: bounded regular input, fresh output inode. A
/// malicious entry is refused instead of being opened for writing.
pub(crate) fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if !in_run_dir(path) {
        return std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(bytes);
    }
    let (dir, name) = Directory::parent(path)?;
    let mut content = match dir.read_file(name) {
        Ok(file) => read_bounded(file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    content.extend_from_slice(bytes);
    if content.len() as u64 > FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "run log exceeds 64 MiB",
        ));
    }
    dir.write(name, &content).map(drop)
}
pub(crate) fn read_bounded(file: File) -> io::Result<Vec<u8>> {
    if file.metadata()?.len() > FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "run file exceeds 64 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "run file exceeds 64 MiB",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: NUL terminated path; test owns its temporary directory.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn run_material_never_reads_special_files_and_writes_new_inodes() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("runs/run");
        fs::create_dir_all(&run).unwrap();
        let outside = temp.path().join("auth.json");
        fs::write(&outside, b"secret").unwrap();
        let dir = Directory::open(&run).unwrap();
        for name in [
            "idle.json",
            "receipt.json",
            "request-000001.json.tmp",
            "exit",
            "limits.json",
            "turn-000002.jsonl",
        ] {
            symlink(&outside, run.join(name)).unwrap();
            assert!(dir.read_file(name).is_err(), "{name}");
            dir.write(name, b"safe").unwrap();
            assert_eq!(fs::read(run.join(name)).unwrap(), b"safe");
            assert_eq!(fs::read(&outside).unwrap(), b"secret");
        }
        fs::hard_link(&outside, run.join("hardlink")).unwrap();
        dir.write("hardlink", b"safe").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        fifo(&run.join("fifo"));
        fs::create_dir(run.join("dir")).unwrap();
        assert!(dir.read_file("fifo").is_err());
        assert!(dir.read_file("dir").is_err());
        dir.write("fifo", b"safe").unwrap();
        assert!(dir.write("dir", b"safe").is_err());
        assert!(
            !dir.names()
                .unwrap()
                .iter()
                .any(|n| n.as_bytes().starts_with(b".dagq-"))
        );
        let large = dir.write("large", b"").unwrap();
        large.set_len(FILE_BYTES + 1).unwrap();
        assert!(read_bounded(dir.read_file("large").unwrap()).is_err());
        for name in ["", ".", "..", "../auth.json", "nested/file"] {
            assert!(dir.write(name, b"unsafe").is_err());
            assert!(dir.mkdir(name).is_err());
        }
    }

    #[test]
    fn run_and_turns_links_cannot_redirect_any_operation() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("runs/run");
        let outside = temp.path().join("outside");
        fs::create_dir_all(run.join("turns")).unwrap();
        fs::create_dir_all(outside.join("turns")).unwrap();
        fs::write(outside.join("turns/request-000001.json"), b"secret").unwrap();
        let pinned = Directory::open(&run.join("turns")).unwrap();
        fs::rename(run.join("turns"), run.join("original")).unwrap();
        symlink(outside.join("turns"), run.join("turns")).unwrap();
        assert!(Directory::open(&run.join("turns")).is_err());
        assert!(create_file(&run.join("turns/exit")).is_err());
        assert!(read_file(&run.join("turns/request-000001.json")).is_err());
        pinned.write("exit", b"").unwrap();
        assert!(run.join("original/exit").is_file());
        assert!(!outside.join("turns/exit").exists());
        fs::rename(&run, temp.path().join("original-run")).unwrap();
        symlink(&outside, &run).unwrap();
        assert!(Directory::open(&run).is_err());
        assert!(Directory::open(&run.join("turns")).is_err());
        assert!(create_file(&run.join("receipt.json")).is_err());
        assert!(read_file(&run.join("turns/request-000001.json")).is_err());
        assert_eq!(
            fs::read(outside.join("turns/request-000001.json")).unwrap(),
            b"secret"
        );
    }

    #[test]
    fn diagnostic_appends_and_tree_operations_stay_with_the_open_directory() {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("runs/run");
        fs::create_dir_all(run.join("broker/nested")).unwrap();
        let secret = temp.path().join("secret");
        fs::write(&secret, b"secret").unwrap();
        let log = run.join("refusals.log");
        append(&log, b"one\n").unwrap();
        append(&log, b"two\n").unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"one\ntwo\n");
        fs::remove_file(&log).unwrap();
        symlink(&secret, &log).unwrap();
        assert!(append(&log, b"unsafe").is_err());
        symlink(&secret, run.join("broker/link")).unwrap();
        symlink(temp.path(), run.join("broker/nested/linkdir")).unwrap();
        // Linux permits non-UTF-8 names; APFS rejects them at creation.
        #[cfg(target_os = "linux")]
        let non_utf8 = OsString::from_vec(vec![0xff]);
        #[cfg(not(target_os = "linux"))]
        let non_utf8 = OsString::from("日本語");
        fs::write(run.join("broker").join(&non_utf8), b"file").unwrap();
        let dir = Directory::open(&run).unwrap();
        let broker = dir.child("broker").unwrap();
        assert!(broker.names().unwrap().contains(&non_utf8));
        assert_eq!(
            kind_of_mode(broker.stat(Some(OsStr::new("link"))).unwrap().st_mode),
            EntryKind::Link
        );
        dir.remove_tree("broker").unwrap();
        assert!(!run.join("broker").exists());
        assert_eq!(fs::read(&secret).unwrap(), b"secret");
    }

    #[test]
    fn an_output_descriptor_stays_on_its_inode_after_the_path_is_swapped() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("runs/run")).unwrap();
        let output = temp.path().join("runs/run/out");
        let outside = temp.path().join("secret");
        fs::write(&outside, b"secret").unwrap();
        let mut file = create_file(&output).unwrap();
        fs::rename(&output, temp.path().join("runs/run/original")).unwrap();
        symlink(&outside, &output).unwrap();
        file.write_all(b"output").unwrap();
        assert_eq!(
            fs::read(temp.path().join("runs/run/original")).unwrap(),
            b"output"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
    }

    /// The host's directories above a run directory may be links (`/tmp`
    /// on macOS, a linked data dir): they are followed, and only a run
    /// directory and what is in it are refused as links.
    #[test]
    fn links_above_the_run_directories_are_the_hosts_and_followed() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        fs::create_dir_all(data.join("queue")).unwrap();
        let linked = temp.path().join("linked");
        symlink(&data, &linked).unwrap();
        let queue = linked.join("queue");
        assert!(Directory::open(&queue).is_ok());
        assert!(Directory::open(&linked).is_ok());
        create_file(&queue.join("repository")).unwrap();
        assert!(data.join("queue/repository").is_file());
        assert!(in_run_dir(&queue.join("runs/a")));
        assert!(in_run_dir(&queue.join("runs/a/turns/exit")));
        assert!(!in_run_dir(&queue.join("runs")));
        assert!(!in_run_dir(&queue.join("repository")));
        fs::create_dir_all(data.join("queue/runs/a")).unwrap();
        symlink(data.join("queue/runs/a"), data.join("queue/runs/b")).unwrap();
        assert!(Directory::open(&queue.join("runs/a")).is_ok());
        assert!(Directory::open(&queue.join("runs/b")).is_err());
        assert!(Directory::open(&queue.join("runs/b/turns")).is_err());
    }

    fn opened(dir: &Path) -> Box<dyn AgentDirHandle> {
        match open(dir).unwrap() {
            AgentDir::Open(handle) => handle,
            _ => panic!("{} did not open", dir.display()),
        }
    }

    /// Its entries with what each is, by name.
    fn entries(handle: &dyn AgentDirHandle) -> Vec<(String, EntryKind)> {
        let mut entries: Vec<(String, EntryKind)> = handle
            .names()
            .unwrap()
            .into_iter()
            .filter_map(|name| Some((name.clone(), handle.kind(&name).unwrap()?)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Somewhere else, with a request-looking file that must stay.
    fn elsewhere(dir: &Path) -> std::path::PathBuf {
        let target = dir.join("elsewhere");
        fs::create_dir_all(target.join("requests")).unwrap();
        fs::write(target.join("secret.json"), "keep").unwrap();
        fs::write(target.join("requests/b.json"), "keep").unwrap();
        target
    }

    #[test]
    fn a_link_or_a_file_in_place_of_the_directory_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let target = elsewhere(dir.path());
        let run = dir.path().join("run");
        fs::create_dir(&run).unwrap();
        symlink(&target, run.join("link")).unwrap();
        assert!(matches!(
            open(&run.join("link")).unwrap(),
            AgentDir::Not(EntryKind::Link)
        ));
        fs::write(run.join("file"), "x").unwrap();
        assert!(matches!(
            open(&run.join("file")).unwrap(),
            AgentDir::Not(EntryKind::File)
        ));
        assert!(matches!(
            open(&run.join("none")).unwrap(),
            AgentDir::Missing
        ));
        assert!(matches!(
            open(&dir.path().join("no-run/requests")).unwrap(),
            AgentDir::Missing
        ));
        assert_eq!(
            fs::read_to_string(target.join("secret.json")).unwrap(),
            "keep"
        );
    }

    /// The agent writes the run directory too: one it swapped for a link
    /// (to another run's) is not followed to the directory in it.
    #[test]
    fn a_link_in_place_of_the_parent_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = elsewhere(dir.path());
        let run = dir.path().join("run");
        symlink(&target, &run).unwrap();
        assert!(matches!(
            open(&run.join("requests")).unwrap(),
            AgentDir::ParentNot(EntryKind::Link)
        ));
        assert!(target.join("requests/b.json").is_file());
    }

    #[test]
    fn entries_are_what_they_are_and_only_regular_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.json");
        fs::write(&outside, "keep").unwrap();
        let requests = dir.path().join("requests");
        fs::create_dir(&requests).unwrap();
        fs::write(requests.join("a.json"), "{}").unwrap();
        symlink(&outside, requests.join("link.json")).unwrap();
        fs::create_dir(requests.join("dir.json")).unwrap();
        let handle = opened(&requests);
        assert_eq!(
            entries(&*handle),
            [
                ("a.json".to_owned(), EntryKind::File),
                ("dir.json".to_owned(), EntryKind::Dir),
                ("link.json".to_owned(), EntryKind::Link),
            ]
        );
        assert_eq!(handle.kind("gone.json").unwrap(), None);
        assert_eq!(handle.read("a.json", 16).unwrap(), b"{}");
        assert!(handle.read("a.json", 1).is_err(), "over the limit");
        assert!(handle.read("link.json", 16).is_err(), "not through a link");
        assert!(handle.read("dir.json", 16).is_err());
        for name in ["", ".", "..", "../outside.json", "x/y"] {
            assert!(handle.read(name, 16).is_err(), "{name:?}");
            assert!(handle.kind(name).is_err(), "{name:?}");
            assert!(handle.rename(name, "z").is_err(), "{name:?}");
            assert!(handle.remove(name).is_err(), "{name:?}");
        }
        // The link is renamed and removed itself, its target untouched.
        handle.rename("link.json", "link.taken").unwrap();
        handle.remove("link.taken").unwrap();
        handle.rename("a.json", "a.taken").unwrap();
        assert!(requests.join("a.taken").is_file());
        assert!(handle.remove("dir.json").is_err(), "not a directory");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    }

    #[test]
    fn the_handle_stays_on_the_directory_it_opened() {
        let dir = tempfile::tempdir().unwrap();
        let requests = dir.path().join("requests");
        fs::create_dir(&requests).unwrap();
        fs::write(requests.join("a.json"), "{}").unwrap();
        let handle = opened(&requests);
        // Swapped for a link to somewhere else after it was opened.
        let target = elsewhere(dir.path());
        fs::rename(&requests, dir.path().join("moved")).unwrap();
        symlink(target.join("requests"), &requests).unwrap();
        assert_eq!(handle.names().unwrap(), ["a.json"]);
        assert!(handle.rename("b.json", "b.taken").is_err());
        assert!(target.join("requests/b.json").is_file());
    }
}
