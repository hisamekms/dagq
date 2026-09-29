//! The fs backend ([Broker] "mountと閉じ込め"): `fs.read`, `fs.list`,
//! `fs.write` and `fs.edit`, each on one path inside the token's workspace.
//!
//! The server has already confined the path by its letters
//! (`TokenClaims::confine`: no `..`, nothing outside the workspace). This
//! backend then opens it without following any symlink: from the mounted
//! root that holds the workspace, every component is opened with `openat`
//! and `O_NOFOLLOW` relative to the directory opened before it, so what is
//! checked is what is opened (no window to swap a directory for a symlink),
//! and a symlink anywhere on the way, pointing in or out, is
//! `workspace_violation`. The workspace's own `.git` (the worktree's gitdir
//! file) and anything under it is `workspace_violation` too, compared
//! without case (APFS does not tell case apart); only the git backend
//! touches the repository.
//!
//! Content is bounded by `Limits::fs_limit_bytes` both ways (`output_limit`),
//! and writes are atomic: a temporary file in the same directory, synced,
//! then renamed over the target.
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

use dagq_broker_protocol::{ErrorCode, encode, fs};

use crate::backend::{Backend, BackendRequest, Call, Done, Failure};

/// The fs backend over the server's mounted roots.
#[derive(Debug, Clone)]
pub struct FsBackend {
    roots: Vec<PathBuf>,
}

impl FsBackend {
    /// A backend that opens workspaces under `roots` (`--root`).
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }
}

impl Backend for FsBackend {
    fn call(&self, call: &Call<'_>, request: BackendRequest) -> Result<Done, Failure> {
        let workspace = Path::new(&call.claims.workspace);
        let [path] = call.confined.as_slice() else {
            return Err(invalid("an fs request names exactly one path"));
        };
        let target = Target::new(workspace, path)?;
        let limit = call.limits.fs_limit_bytes;
        let root = open_workspace(&self.roots, workspace)?;
        let body = match request {
            BackendRequest::FsRead(request) => read(&root, &target, &request, limit)?,
            BackendRequest::FsList(_) => list(&root, &target, limit)?,
            BackendRequest::FsWrite(request) => write(&root, &target, &request, limit)?,
            BackendRequest::FsEdit(request) => edit(&root, &target, &request, limit)?,
            _ => {
                return Err(invalid(format!(
                    "{} is not an fs operation",
                    call.operation
                )));
            }
        };
        Ok(Done {
            body,
            exit_code: None,
        })
    }
}

/// The workspace's directory, opened from the mounted root under it
/// without following a symlink below the root. The process backend runs its
/// programs in it too.
pub(crate) fn open_workspace(roots: &[PathBuf], workspace: &Path) -> Result<OwnedFd, Failure> {
    let root = roots
        .iter()
        .filter(|root| workspace.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .ok_or_else(|| violation("the token's workspace is not under a mounted root"))?;
    let mut dir = open_root(root).map_err(|error| {
        Failure::new(
            ErrorCode::BackendError,
            format!("open the mounted root: {error}"),
        )
    })?;
    for part in normal_parts(workspace.strip_prefix(root).unwrap_or(Path::new("")))? {
        dir = step(&dir, &part, false, "the workspace")?;
    }
    Ok(dir)
}

/// The request's path as the names below the workspace.
#[derive(Debug)]
struct Target {
    parts: Vec<OsString>,
    /// Relative to the workspace, `.` for the workspace itself, for messages.
    shown: String,
}

impl Target {
    fn new(workspace: &Path, path: &Path) -> Result<Self, Failure> {
        let relative = path
            .strip_prefix(workspace)
            .map_err(|_| violation("the path is outside the workspace"))?;
        let parts = normal_parts(relative)?;
        if parts
            .first()
            .is_some_and(|first| first.as_bytes().eq_ignore_ascii_case(b".git"))
        {
            return Err(violation(
                "the worktree's .git is only for the git operations",
            ));
        }
        let shown = if parts.is_empty() {
            ".".to_owned()
        } else {
            relative.to_string_lossy().into_owned()
        };
        Ok(Self { parts, shown })
    }

    /// The directory that holds the target and the target's name; creating
    /// the missing directories on the way when `create`.
    fn parent(&self, root: &OwnedFd, create: bool) -> Result<(OwnedFd, &OsStr), Failure> {
        let Some((name, dirs)) = self.parts.split_last() else {
            return Err(backend(format!(
                "{} is the workspace, a directory",
                self.shown
            )));
        };
        let mut dir = dup(root)?;
        for part in dirs {
            dir = step(&dir, part, create, &self.shown)?;
        }
        Ok((dir, name))
    }

    /// The target, opened as a directory.
    fn dir(&self, root: &OwnedFd) -> Result<OwnedFd, Failure> {
        let mut dir = dup(root)?;
        for part in &self.parts {
            dir = step(&dir, part, false, &self.shown)?;
        }
        Ok(dir)
    }
}

/// The components of `relative`, which must all be names (the server's
/// confinement has already refused `..` and roots).
fn normal_parts(relative: &Path) -> Result<Vec<OsString>, Failure> {
    relative
        .components()
        .filter(|component| *component != Component::CurDir)
        .map(|component| match component {
            Component::Normal(part) => Ok(part.to_os_string()),
            _ => Err(violation("the path leaves the workspace")),
        })
        .collect()
}

fn read(
    root: &OwnedFd,
    target: &Target,
    request: &fs::ReadRequest,
    limit: u64,
) -> Result<Vec<u8>, Failure> {
    let (dir, name) = target.parent(root, false)?;
    let (file, _) = open_regular(&dir, name, &target.shown)?;
    let mut reader = BufReader::new(file);
    let io = |error: io::Error| backend(format!("read {}: {error}", target.shown));
    for _ in 0..request.offset.unwrap_or(0) {
        if !take_line(&mut reader, None, limit).map_err(io)? {
            break;
        }
    }
    let mut content = Vec::new();
    let mut lines = 0;
    while lines < request.limit.unwrap_or(u64::MAX) {
        let more = take_line(&mut reader, Some(&mut content), limit).map_err(|error| {
            if error.kind() == io::ErrorKind::FileTooLarge {
                over_limit(&target.shown, limit, "read; ask for fewer lines")
            } else {
                io(error)
            }
        })?;
        if !more {
            break;
        }
        lines += 1;
    }
    let truncated = !reader.fill_buf().map_err(io)?.is_empty();
    let content = String::from_utf8(content)
        .map_err(|_| backend(format!("{} is not UTF-8 text", target.shown)))?;
    answer(&fs::ReadResponse {
        content,
        lines,
        truncated,
    })
}

/// Read one line (with its `\n`) into `into`, or past it when `into` is
/// `None`; `false` at the end of the file. More than `limit` bytes in
/// `into` is an error of kind `FileTooLarge`.
fn take_line(
    reader: &mut impl BufRead,
    mut into: Option<&mut Vec<u8>>,
    limit: u64,
) -> io::Result<bool> {
    let mut any = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(any);
        }
        any = true;
        let (take, done) = match buf.iter().position(|byte| *byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (buf.len(), false),
        };
        if let Some(into) = into.as_deref_mut() {
            if (into.len() + take) as u64 > limit {
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            into.extend_from_slice(&buf[..take]);
        }
        reader.consume(take);
        if done {
            return Ok(true);
        }
    }
}

fn list(root: &OwnedFd, target: &Target, limit: u64) -> Result<Vec<u8>, Failure> {
    let dir = target.dir(root)?;
    let mut entries = Vec::new();
    for name in
        dir_names(&dir).map_err(|error| backend(format!("list {}: {error}", target.shown)))?
    {
        let (kind, size) = match lstat_at(&dir, &name) {
            Ok(stat) => match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => (fs::EntryKind::File, stat.st_size as u64),
                libc::S_IFDIR => (fs::EntryKind::Dir, 0),
                libc::S_IFLNK => (fs::EntryKind::Symlink, 0),
                _ => (fs::EntryKind::Other, 0),
            },
            // Gone since it was listed.
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => continue,
            Err(error) => return Err(backend(format!("list {}: {error}", target.shown))),
        };
        entries.push(fs::Entry {
            name: name.to_string_lossy().into_owned(),
            kind,
            size,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let body = answer(&fs::ListResponse { entries })?;
    if body.len() as u64 > limit {
        return Err(over_limit(&target.shown, limit, "listed"));
    }
    Ok(body)
}

fn write(
    root: &OwnedFd,
    target: &Target,
    request: &fs::WriteRequest,
    limit: u64,
) -> Result<Vec<u8>, Failure> {
    if request.content.len() as u64 > limit {
        return Err(over_limit(&target.shown, limit, "written"));
    }
    let (dir, name) = target.parent(root, request.create_dirs)?;
    let mode = match lstat_at(&dir, name) {
        Ok(stat) => Some(regular_mode(&stat, &target.shown)?),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => None,
        Err(error) => return Err(backend(format!("write {}: {error}", target.shown))),
    };
    replace(&dir, name, request.content.as_bytes(), mode, &target.shown)?;
    answer(&fs::WriteResponse {
        bytes: request.content.len() as u64,
    })
}

fn edit(
    root: &OwnedFd,
    target: &Target,
    request: &fs::EditRequest,
    limit: u64,
) -> Result<Vec<u8>, Failure> {
    if request.old_string.is_empty() {
        return Err(invalid("old_string is empty"));
    }
    if request.old_string == request.new_string {
        return Err(invalid("old_string and new_string are the same"));
    }
    let (dir, name) = target.parent(root, false)?;
    let (file, stat) = open_regular(&dir, name, &target.shown)?;
    if stat.st_size as u64 > limit {
        return Err(over_limit(&target.shown, limit, "edited"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| backend(format!("read {}: {error}", target.shown)))?;
    if bytes.len() as u64 > limit {
        return Err(over_limit(&target.shown, limit, "edited"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| backend(format!("{} is not UTF-8 text", target.shown)))?;
    let matches = text.matches(request.old_string.as_str()).count();
    if matches == 0 {
        return Err(invalid(format!(
            "old_string was not found in {}",
            target.shown
        )));
    }
    if matches > 1 && !request.replace_all {
        return Err(invalid(format!(
            "old_string matches {matches} times in {}; give more context to make it unique, or set replace_all",
            target.shown
        )));
    }
    let edited = if request.replace_all {
        text.replace(&request.old_string, &request.new_string)
    } else {
        text.replacen(&request.old_string, &request.new_string, 1)
    };
    if edited.len() as u64 > limit {
        return Err(over_limit(&target.shown, limit, "written"));
    }
    let mode = regular_mode(&stat, &target.shown)?;
    replace(&dir, name, edited.as_bytes(), Some(mode), &target.shown)?;
    answer(&fs::EditResponse {
        replacements: if request.replace_all {
            matches as u64
        } else {
            1
        },
    })
}

/// The name of `replace`'s temporary file is `TEMPORARY_PREFIX<uuid>TEMPORARY_SUFFIX`.
/// A broker that stops between creating and renaming it leaves it behind;
/// `git.add` never stages such a file.
pub(crate) const TEMPORARY_PREFIX: &str = ".dagq-broker-";
pub(crate) const TEMPORARY_SUFFIX: &str = ".tmp";

/// Whether `name` (one path component) is a temporary file of `replace`.
pub(crate) fn is_temporary(name: &[u8]) -> bool {
    name.len() > TEMPORARY_PREFIX.len() + TEMPORARY_SUFFIX.len()
        && name.starts_with(TEMPORARY_PREFIX.as_bytes())
        && name.ends_with(TEMPORARY_SUFFIX.as_bytes())
}

/// Put `content` at `name` in `dir` atomically: a new temporary file in the
/// same directory, synced, then renamed over the target. `mode` is the
/// replaced file's permission bits, kept; a new file gets `0666` less the
/// umask.
fn replace(
    dir: &OwnedFd,
    name: &OsStr,
    content: &[u8],
    mode: Option<libc::mode_t>,
    shown: &str,
) -> Result<(), Failure> {
    let temporary = OsString::from(format!(
        "{TEMPORARY_PREFIX}{}{TEMPORARY_SUFFIX}",
        uuid::Uuid::new_v4()
    ));
    let failed = |error: io::Error| backend(format!("write {shown}: {error}"));
    let fd = open_at(
        dir,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o666,
    )
    .map_err(failed)?;
    let written = (|| {
        if let Some(mode) = mode {
            // SAFETY: fchmod on a descriptor this function owns.
            if unsafe { libc::fchmod(fd.as_raw_fd(), mode) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut file = File::from(fd);
        file.write_all(content)?;
        file.sync_all()?;
        rename_at(dir, &temporary, name)
    })();
    written.map_err(|error| {
        let _ = unlink_at(dir, &temporary);
        failed(error)
    })
}

/// Open the regular file `name` in `dir` for reading, not following a
/// symlink, not blocking on a FIFO and not taking a terminal.
pub(crate) fn open_regular(
    dir: &OwnedFd,
    name: &OsStr,
    shown: &str,
) -> Result<(File, libc::stat), Failure> {
    let fd = open_at(
        dir,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        0,
    )
    .map_err(|error| refused(dir, name, error, shown))?;
    let stat = fstat(&fd).map_err(|error| backend(format!("read {shown}: {error}")))?;
    regular_mode(&stat, shown)?;
    Ok((File::from(fd), stat))
}

/// The permission bits of a regular file; a directory or anything else is
/// `backend_error`, a symlink `workspace_violation`.
fn regular_mode(stat: &libc::stat, shown: &str) -> Result<libc::mode_t, Failure> {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFREG => Ok(stat.st_mode & 0o777),
        libc::S_IFDIR => Err(backend(format!("{shown} is a directory"))),
        libc::S_IFLNK => Err(symlink_violation(shown)),
        _ => Err(backend(format!("{shown} is not a regular file"))),
    }
}

/// Open the directory `name` in `dir`, not following a symlink; create it
/// first when it is missing and `create`.
fn step(dir: &OwnedFd, name: &OsStr, create: bool, shown: &str) -> Result<OwnedFd, Failure> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    match open_at(dir, name, flags, 0) {
        Ok(fd) => Ok(fd),
        Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
            match mkdir_at(dir, name) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                Err(error) => {
                    return Err(backend(format!("create a directory for {shown}: {error}")));
                }
            }
            open_at(dir, name, flags, 0).map_err(|error| refused(dir, name, error, shown))
        }
        Err(error) => Err(refused(dir, name, error, shown)),
    }
}

/// Why opening `name` in `dir` failed: a symlink is `workspace_violation`,
/// anything else `backend_error`.
fn refused(dir: &OwnedFd, name: &OsStr, error: io::Error, shown: &str) -> Failure {
    let is_symlink =
        lstat_at(dir, name).is_ok_and(|stat| stat.st_mode & libc::S_IFMT == libc::S_IFLNK);
    if is_symlink || error.raw_os_error() == Some(libc::ELOOP) {
        return symlink_violation(shown);
    }
    let reason = match error.raw_os_error() {
        Some(libc::ENOENT) => "no such file or directory".to_owned(),
        Some(libc::ENOTDIR) => "not a directory".to_owned(),
        _ => error.to_string(),
    };
    backend(format!("{shown}: {reason}"))
}

fn symlink_violation(shown: &str) -> Failure {
    violation(format!(
        "{shown}: a symlink on the way; the broker does not follow symlinks"
    ))
}

fn over_limit(shown: &str, limit: u64, what: &str) -> Failure {
    Failure::new(
        ErrorCode::OutputLimit,
        format!("{shown}: more than {limit} bytes to be {what}"),
    )
}

fn violation(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::WorkspaceViolation, message)
}

fn backend(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::BackendError, message)
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(ErrorCode::InvalidRequest, message)
}

fn answer<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Failure> {
    encode(value).map_err(|error| backend(error.to_string()))
}

// The system calls, each over a descriptor the caller owns.

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let path = c_name(root.as_os_str())?;
    // SAFETY: a NUL-terminated path; the descriptor is owned at once.
    let fd = cvt(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: `fd` is a new descriptor no one else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_at(
    dir: &OwnedFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    // SAFETY: `dir` is open and `name` NUL-terminated; the descriptor is
    // owned at once.
    let fd = cvt(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) })?;
    // SAFETY: `fd` is a new descriptor no one else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup(fd: &OwnedFd) -> Result<OwnedFd, Failure> {
    fd.try_clone()
        .map_err(|error| backend(format!("duplicate a descriptor: {error}")))
}

fn mkdir_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `dir` is open and `name` NUL-terminated.
    cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) }).map(drop)
}

fn rename_at(dir: &OwnedFd, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    // SAFETY: `dir` is open and both names NUL-terminated.
    cvt(unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) })
        .map(drop)
}

fn unlink_at(dir: &OwnedFd, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: `dir` is open and `name` NUL-terminated.
    cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
}

pub(crate) fn lstat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<libc::stat> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` is open, `name` NUL-terminated and `stat` large enough;
    // it is read only after the call succeeded.
    cvt(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { stat.assume_init() })
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is open and `stat` large enough; it is read only after
    // the call succeeded.
    cvt(unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) })?;
    Ok(unsafe { stat.assume_init() })
}

/// The names in the directory `dir`, without `.` and `..`.
fn dir_names(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    let fd = dir.try_clone()?;
    // SAFETY: `fdopendir` takes over the duplicate on success; on failure it
    // is still ours and dropped.
    let stream = unsafe { libc::fdopendir(fd.as_raw_fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(fd);
    let mut names = Vec::new();
    let mut failed = None;
    loop {
        // `readdir` returns NULL both at the end and on an error, telling
        // them apart by errno only.
        set_errno(0);
        // SAFETY: `stream` is open until `closedir` below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error().is_some_and(|errno| errno != 0) {
                failed = Some(error);
            }
            break;
        }
        // SAFETY: `d_name` of an entry `readdir` returned is NUL-terminated
        // and valid until the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    // SAFETY: closes the stream and the duplicate it owns.
    unsafe { libc::closedir(stream) };
    match failed {
        Some(error) => Err(error),
        None => Ok(names),
    }
}

fn set_errno(value: libc::c_int) {
    // SAFETY: the calling thread's errno, always valid to write.
    unsafe {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            *libc::__error() = value;
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
        {
            *libc::__errno_location() = value;
        }
    }
}

#[cfg(test)]
mod tests;
