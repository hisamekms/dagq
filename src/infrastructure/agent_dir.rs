//! A directory an agent writes, opened without following links
//! ([`AgentDirHandle`]): a Codex worker's ask requests in its run directory
//! (ADR-t813-3 decision 3). The supervisor runs outside the worker's
//! sandbox, so it must not do for the worker what the sandbox stops. The
//! worker writes both the directory and its parent (the run directory), so
//! neither is followed if it is a link: the parent is opened with
//! `O_NOFOLLOW`, the directory relative to it, and the directory's entries
//! are looked at, read, renamed and removed relative to that descriptor
//! (`fstatat`, `openat`, `renameat`, `unlinkat`), never through a path the
//! worker can point elsewhere between a check and its use. The parent's own
//! parent (the queue's `runs/`) is the runtime's, which the worker cannot
//! write.

use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
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
fn entry_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is no entry of the directory"),
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

impl AgentDirHandle for Handle {
    fn names(&self) -> io::Result<Vec<String>> {
        // A stream of its own over a duplicate (not inherited by children),
        // closed with it, read from the start.
        // SAFETY: the descriptor is open.
        let duplicate = unsafe { libc::fcntl(self.fd(), libc::F_DUPFD_CLOEXEC, 0) };
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
                if let Ok(name) = name.to_str()
                    && name != "."
                    && name != ".."
                {
                    names.push(name.to_owned());
                }
            }
            libc::closedir(stream);
        }
        Ok(names)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

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
