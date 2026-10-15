//! The git of a run in a container (ADR-t2113-2, ADR-t2179-1) on the
//! [`GitRepository`] adapter: the run's clone made by the control side
//! ([`GitRepository::create_clone`]), the facts read from the bundle and
//! the state file the container's executor writes
//! ([`GitRepository::clone_facts`]), the landing worktree
//! ([`GitRepository::landing_worktree`]), main's bundle
//! ([`GitRepository::hand_main`]), the removal of what the run left
//! ([`GitRepository::remove_clone`]) and the guard of the control side's
//! git ([`Guard`]). No git runs with the clone as its repository after the
//! clone is made, and no file of the run directory is opened through a
//! symlink.

use std::{
    ffi::{CStr, CString},
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    sync::{Arc, Mutex},
};

use super::*;
use crate::application::CloneFacts;
use crate::domain::run_clone::{
    CLONE_MAIN_REF, GuardDoesNotHold, MAIN_BUNDLE_FILE, RUN_BUNDLE_FILE, RUN_FILE_LIMIT,
    RUN_STATE_FILE, RunState, bundle_head, guard_env, guard_holds, guard_overrides, is_clone_path,
    landing_of, run_dir_of,
};

/// How long a fetch of a run's bundle or a bundle of main may take.
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The configuration a guarded [`GitRepository`] runs every git with
/// ([`guard_overrides`]): no hook, and every driver a configuration scope
/// defines made one that runs no program. It only grows: a directory
/// whose configuration defines more drivers (a conditional include) adds
/// theirs before git runs there.
pub(super) struct Guard {
    overrides: Mutex<Vec<(String, String)>>,
}

impl Guard {
    /// The overrides as a process's environment ([`guard_env`]).
    pub(super) fn env(&self) -> Vec<(String, String)> {
        guard_env(&self.overrides.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// Refuses `worktree` when it is a run's clone: the control side's git
/// never runs there (ADR-t2179-1 decision 1), whichever caller asks.
pub(super) fn refuse_clone(worktree: &Path) -> Result<()> {
    ensure!(
        !is_clone_path(worktree),
        "{} is a container run's clone, which the control side runs no git in",
        worktree.display()
    );
    Ok(())
}

/// What is at `path`, not following a symlink there.
fn entry(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("look at {}", path.display())),
    }
}

/// Remove what is at `path` without following a symlink: a symlink or a
/// file is unlinked, a directory removed with what is in it, none of its
/// symlinks followed. Nothing there is not an error.
fn remove_entry(path: &Path) -> Result<()> {
    let Some(meta) = entry(path)? else {
        return Ok(());
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("remove {}", path.display()))
}

/// Remove the entry `name` of the directory `parent` (shown as `shown` in
/// errors), also when the commands that ran in it left directories nobody
/// may write or read (a module cache of 0555 directories), and while a
/// process still running there may swap a directory for a symlink: every
/// step works from a directory's descriptor and an entry's name
/// (`fstatat`, `openat` with `O_NOFOLLOW`, `unlinkat`), never through a
/// path, so no symlink is followed and nothing outside `parent` is
/// reached. A directory is made its owner's to read, write and enter
/// (`fchmod` on its descriptor, or `fchmodat` with `AT_SYMLINK_NOFOLLOW`
/// on one it cannot open) before what is in it goes. No git runs. Nothing
/// there is not an error.
fn remove_at(parent: BorrowedFd<'_>, name: &CStr, shown: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let failed = |what: &str| {
        let error = io::Error::last_os_error();
        anyhow::Error::new(error).context(format!("{what} {}", shown.display()))
    };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and `stat` is written by the call.
    let looked = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if looked != 0 {
        if io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(failed("look at"));
    }
    // SAFETY: fstatat succeeded, so `stat` is initialized.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        // SAFETY: `name` is NUL-terminated; the entry itself is unlinked.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::NotFound
        {
            return Err(failed("remove"));
        }
        return Ok(());
    }
    let dir = match open_dir_at(parent, name) {
        Ok(dir) => dir,
        // A directory its owner may not read: made its owner's without
        // following a symlink put there meanwhile, then opened again.
        Err(error) if error.raw_os_error() == Some(libc::EACCES) => {
            // SAFETY: `name` is NUL-terminated.
            let changed = unsafe {
                libc::fchmodat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    0o700,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if changed != 0 {
                return Err(failed("make its owner's"));
            }
            open_dir_at(parent, name).with_context(|| format!("open {}", shown.display()))?
        }
        Err(error) => return Err(error).with_context(|| format!("open {}", shown.display())),
    };
    // Write and search, to unlink what is in it.
    // SAFETY: `dir` is an open descriptor.
    if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
        return Err(failed("make its owner's"));
    }
    for child in names_in(dir.as_fd()).with_context(|| format!("read {}", shown.display()))? {
        let path = shown.join(std::ffi::OsStr::from_bytes(child.to_bytes()));
        remove_at(dir.as_fd(), &child, &path)?;
    }
    drop(dir);
    // SAFETY: `name` is NUL-terminated; an empty directory is removed.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(failed("remove"));
    }
    Ok(())
}

/// The directory `name` of `parent`, opened only when it is a directory
/// and not a symlink.
fn open_dir_at(parent: BorrowedFd<'_>, name: &CStr) -> io::Result<OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor this owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The names in the directory `dir` but `.` and `..`, read through a
/// copy of its descriptor.
fn names_in(dir: BorrowedFd<'_>) -> io::Result<Vec<CString>> {
    use std::os::fd::AsRawFd;
    // SAFETY: `dir` is an open descriptor; the copy is the stream's.
    let copy = unsafe { libc::dup(dir.as_raw_fd()) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `copy` is an open directory descriptor, owned by the stream
    // once fdopendir succeeds.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the stream did not take `copy`.
        unsafe { libc::close(copy) };
        return Err(error);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is open; the entry is read before the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name != c"." && name != c".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: `stream` is open and closed once, with `copy`.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// The bytes of the run directory's file at `path`, opened without
/// following a symlink and only when it is a regular file of at most
/// [`RUN_FILE_LIMIT`] bytes (ADR-t2179-1 decision 9); `Ok(None)` when
/// nothing is there, `Err(why)` otherwise.
fn read_run_file(path: &Path) -> std::result::Result<Option<Vec<u8>>, String> {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(format!("{name} of the run is a symlink, which is not read"));
        }
        Err(error) => return Err(format!("{name} of the run cannot be opened: {error}")),
    };
    let meta = file
        .metadata()
        .map_err(|error| format!("{name} of the run cannot be read: {error}"))?;
    if !meta.is_file() {
        return Err(format!("{name} of the run is not a regular file"));
    }
    if meta.len() > RUN_FILE_LIMIT {
        return Err(format!(
            "{name} of the run is larger than {RUN_FILE_LIMIT} bytes"
        ));
    }
    let mut bytes = Vec::new();
    file.take(RUN_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{name} of the run cannot be read: {error}"))?;
    Ok(Some(bytes))
}

/// Write `bytes` as the run directory's file at `path`: whatever is there
/// is removed first without following it, and the file is made new
/// without following a symlink (ADR-t2179-1 decision 9), so a symlink the
/// worker puts there never has the control side write elsewhere.
fn write_run_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut last = None;
    for _ in 0..3 {
        remove_entry(path)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(bytes)
                    .and_then(|()| file.sync_all())
                    .with_context(|| format!("write {}", path.display()))?;
                return Ok(());
            }
            // Something came back meanwhile: remove it again.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    Err(last.expect("tried")).with_context(|| format!("create {}", path.display()))
}

/// A file of the control side's own, removed when dropped: the copy of a
/// run's bundle git reads, out of the worker's reach.
struct Scratch(PathBuf);

impl Scratch {
    fn new(bytes: &[u8]) -> Result<Self> {
        let path = env::temp_dir().join(format!("dagq-run-bundle-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        let scratch = Self(path);
        file.write_all(bytes)
            .with_context(|| format!("write {}", scratch.0.display()))?;
        Ok(scratch)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Whether `a` and `b` name one path, through their canonical forms when
/// both have one.
fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (a.canonicalize(), b.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
}

impl GitRepository {
    /// This repository guarded ([`Guard`]): its overrides read from every
    /// configuration scope at its root, and checked to hold there.
    pub fn guarded_repository(&self) -> Result<Self> {
        let mut guarded = self.clone();
        guarded.guard = Some(Arc::new(Guard {
            overrides: Mutex::new(self.overrides_in(&self.root)?),
        }));
        guarded.check_guard(&self.root)?;
        Ok(guarded)
    }

    /// The overrides for the configuration git reads in `dir`.
    fn overrides_in(&self, dir: &Path) -> Result<Vec<(String, String)>> {
        let names = output(
            Command::new(&self.git)
                .env_remove("GIT_CONFIG_PARAMETERS")
                .arg("-C")
                .arg(dir)
                .args(["config", "--list", "--name-only", "-z"]),
        )
        .with_context(|| format!("read the git configuration in {}", dir.display()))?;
        guard_overrides(&names).map_err(|why| anyhow::Error::new(GuardDoesNotHold(why)))
    }

    /// Add to the guard the overrides `dir` needs and check that they hold
    /// there: a guarded repository only.
    fn widen_guard(&self, dir: &Path) -> Result<()> {
        let guard = self
            .guard
            .as_ref()
            .context("the repository is not guarded")?;
        let more = self.overrides_in(dir)?;
        {
            let mut overrides = guard.overrides.lock().unwrap_or_else(|e| e.into_inner());
            for (key, value) in more {
                if !overrides.iter().any(|(k, _)| *k == key) {
                    overrides.push((key, value));
                }
            }
        }
        self.check_guard(dir)
    }

    /// Whether every override is what git reads in `dir`: `Err` when one is
    /// not, and nothing of a container run is to run then.
    fn check_guard(&self, dir: &Path) -> Result<()> {
        let guard = self
            .guard
            .as_ref()
            .context("the repository is not guarded")?;
        let effective = output(
            self.command()
                .arg("-C")
                .arg(dir)
                .args(["config", "--list", "-z"]),
        )
        .with_context(|| format!("read the guarded configuration in {}", dir.display()))?;
        let overrides = guard
            .overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        guard_holds(&effective, &overrides).map_err(|why| {
            anyhow::Error::new(GuardDoesNotHold(format!(
                "the hooks and drivers of the git configuration in {} cannot be turned off: {why}",
                dir.display()
            )))
        })
    }

    /// See [`Repository::guard_env`].
    pub fn guard_env_in(&self, cwd: &Path) -> Result<Vec<(String, String)>> {
        let guarded = self.guarded_repository()?;
        guarded.widen_guard(cwd)?;
        Ok(guarded.guard.as_ref().expect("guarded").env())
    }

    /// See [`Repository::create_clone`]. The clone is made by the control
    /// side with git there while it holds only what the control side put
    /// in it, every command guarded; the base comes over `file://`, which
    /// sends a pack rather than linking object files. A clone an earlier
    /// attempt left is removed first while the queue records no worker
    /// started for the run, and one this attempt leaves half made is
    /// removed before its error returns, both as files.
    pub fn create_clone(&self, run: &TaskRun, carried: Option<&str>) -> Result<String> {
        let clone = Path::new(run.worktree_path().context("missing worktree")?);
        ensure!(run.in_container(), "run {} has no clone", run.id());
        let guarded = self.guarded_repository()?;
        if let Some(parent) = clone.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        if entry(clone)?.is_some() {
            // What a worker has had is untrusted: whether it may go is read
            // from the queue's record, never from the clone.
            ensure!(
                run.workspace_id().is_none(),
                "{} is there already and run {}'s worker has been started on it",
                clone.display(),
                run.id()
            );
            remove_entry(clone)?;
        }
        let made = guarded.fill_clone(run, clone, carried);
        if made.is_err()
            && let Err(error) = remove_entry(clone)
        {
            tracing::warn!(error = %format_args!("{error:#}"), "the half made clone {} could not be removed: {error:#}", clone.display());
        }
        made
    }

    /// Make the clone at `clone` and the control side's run branch, with
    /// this repository guarded.
    fn fill_clone(&self, run: &TaskRun, clone: &Path, carried: Option<&str>) -> Result<String> {
        let branch = run.branch().context("missing branch")?;
        let base = run.base_commit().as_str();
        let guarded = self;
        let mut log = output(
            guarded
                .command()
                .args(["init", "--quiet", "--template="])
                .arg(clone),
        )?;
        let in_clone = || {
            let mut command = guarded.command();
            command.arg("-C").arg(clone);
            command
        };
        let mut fetch = in_clone();
        fetch
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
            .arg(format!("file://{}", self.root.display()))
            .arg(format!("{base}:refs/heads/{branch}"));
        if let Some(carried) = carried {
            fetch.arg(format!("{carried}:{carried}"));
        }
        log.push_str(&output(&mut fetch)?);
        output(in_clone().args(["update-ref", CLONE_MAIN_REF, base]))?;
        output(in_clone().args(["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]))?;
        log.push_str(&output(in_clone().args(["reset", "--quiet", "--hard"]))?);
        // The worker commits under the name the repository commits under.
        for key in ["user.name", "user.email"] {
            let (status, value, _) = capture(
                guarded.git_root().args(["config", "--get", key]),
                OUTPUT_TIMEOUT,
            )?;
            if status.success() && !value.trim().is_empty() {
                output(in_clone().args(["config", key, value.trim()]))?;
            }
        }
        output(
            guarded
                .git_root()
                .args(["update-ref", &format!("refs/heads/{branch}"), base]),
        )?;
        Ok(log)
    }

    /// Make the directory of a verification in the container at `dir`
    /// (ADR-t2173-1 decision 1, ADR-t2179-1 decisions 5 and 8), holding
    /// the clone of `commit` at
    /// [`VERIFICATION_CLONE`](crate::domain::run_container::VERIFICATION_CLONE),
    /// made over `file://` with every git guarded so no tracked hook and no
    /// driver `.gitattributes` selects runs, the drivers a configuration
    /// includes only for the clone (its gitdir, its branch) among them. The parent of `dir` (the
    /// queue's verification directory, which verifications share) is made
    /// when it is not there, and when it is, it is to be a directory, no
    /// symlink, the control side's and writable by neither its group nor
    /// others; whatever is at `dir` already (a file, a directory, a
    /// symlink) is refused, and nothing is made. A directory this leaves
    /// half made is removed before its error returns.
    pub fn create_verification_clone(&self, dir: &Path, commit: &CommitSha) -> Result<PathBuf> {
        let parent = dir.parent().context("no verification directory")?;
        let queue_dir = parent.parent().context("no queue directory")?;
        match entry(queue_dir)? {
            Some(meta) if meta.is_dir() && !meta.file_type().is_symlink() => (),
            _ => bail!(
                "the queue's directory {} is a symlink, no directory or gone, so no verification is made in it",
                queue_dir.display()
            ),
        }
        match entry(parent)? {
            None => DirBuilder::new()
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("create {}", parent.display()))?,
            Some(meta) if meta.file_type().is_symlink() || !meta.is_dir() => bail!(
                "{} is a symlink or no directory, so no verification is made in it",
                parent.display()
            ),
            // SAFETY: geteuid reads the process's own effective user ID.
            Some(meta) if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 => {
                bail!(
                    "{} is not the control side's alone (another owner, or writable by its group or others), so no verification is made in it",
                    parent.display()
                )
            }
            Some(_) => (),
        }
        ensure!(
            entry(dir)?.is_none(),
            "{} is there already, so no verification is made there",
            dir.display()
        );
        // Fails when something came there meanwhile.
        DirBuilder::new()
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let clone = dir.join(crate::domain::run_container::VERIFICATION_CLONE);
        let made = self
            .guarded_repository()
            .and_then(|guarded| guarded.fill_verification_clone(&clone, commit));
        if let Err(error) = made {
            if let Err(left) = remove_entry(dir) {
                tracing::warn!(error = %format_args!("{left:#}"), "the half made verification {} could not be removed: {left:#}", dir.display());
            }
            return Err(error);
        }
        Ok(clone)
    }

    /// Fill the verification's clone at `clone` with `commit`, checked out
    /// on a branch of the clone's own, with this repository guarded.
    fn fill_verification_clone(&self, clone: &Path, commit: &CommitSha) -> Result<()> {
        const BRANCH: &str = "refs/heads/verification";
        output(
            self.command()
                .args(["init", "--quiet", "--template="])
                .arg(clone),
        )?;
        // A configuration only the clone reads (a conditional include on
        // its gitdir) is guarded before git runs there.
        self.widen_guard(clone)?;
        let in_clone = || {
            let mut command = self.command();
            command.arg("-C").arg(clone);
            command
        };
        output(
            in_clone()
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
                .arg(format!("file://{}", self.root.display()))
                .arg(format!("{commit}:{BRANCH}")),
        )?;
        output(in_clone().args(["symbolic-ref", "HEAD", BRANCH]))?;
        // And one on its branch, before its files are written.
        self.widen_guard(clone)?;
        output(in_clone().args(["reset", "--quiet", "--hard"]))?;
        Ok(())
    }

    /// Remove a verification's directory at `dir` without git and
    /// following no symlink (ADR-t2179-1 decision 7), from the descriptor
    /// of its parent ([`remove_at`]), so a container that could not be
    /// removed and still runs there reaches nothing outside it by swapping
    /// a directory for a symlink: nothing is removed through a parent that
    /// is a symlink or no directory.
    pub fn remove_verification_dir(dir: &Path) -> Result<()> {
        if let Some(meta) = entry(dir.parent().context("no verification directory")?)?
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            bail!(
                "{} is a symlink or no directory; nothing is removed through it",
                dir.parent().unwrap_or(dir).display()
            );
        }
        let parent = dir.parent().context("no verification directory")?;
        let parent = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent)
        {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("open {}", parent.display()));
            }
        };
        let name = CString::new(
            dir.file_name()
                .context("no verification's name")?
                .as_bytes(),
        )
        .with_context(|| format!("{} holds a NUL", dir.display()))?;
        remove_at(parent.as_fd(), &name, dir)
    }

    /// See [`Repository::clone_facts`].
    pub fn clone_facts(&self, run: &TaskRun) -> Result<std::result::Result<CloneFacts, String>> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let branch = run.branch().context("missing branch")?;
        let state = match read_run_file(&run_dir.join(RUN_STATE_FILE)) {
            Ok(Some(bytes)) => match RunState::parse(&String::from_utf8_lossy(&bytes)) {
                Ok(state) => state,
                Err(why) => return Ok(Err(why)),
            },
            Ok(None) => {
                return Ok(Err(format!(
                    "the run's state file ({RUN_STATE_FILE}) is missing from its directory"
                )));
            }
            Err(why) => return Ok(Err(why)),
        };
        let guarded = self.guarded_repository()?;
        let reference = format!("refs/heads/{branch}");
        match read_run_file(&run_dir.join(RUN_BUNDLE_FILE)) {
            // No bundle is a run branch with no commit of its own, which the
            // state must say too.
            Ok(None) if state.head() == *run.base_commit() => {
                output(guarded.git_root().args([
                    "update-ref",
                    &reference,
                    run.base_commit().as_str(),
                ]))?;
            }
            Ok(None) => {
                return Ok(Err(format!(
                    "the run bundle ({RUN_BUNDLE_FILE}) is missing from the run's directory, though the run's state names {} as HEAD",
                    state.head()
                )));
            }
            Err(why) => return Ok(Err(why)),
            Ok(Some(bytes)) => {
                let scratch = Scratch::new(&bytes)?;
                let (status, heads, stderr) = capture(
                    guarded
                        .git_root()
                        .args(["bundle", "list-heads"])
                        .arg(&scratch.0),
                    OUTPUT_TIMEOUT,
                )?;
                if !status.success() {
                    return Ok(Err(format!(
                        "the run bundle ({RUN_BUNDLE_FILE}) cannot be read: {}",
                        stderr.trim()
                    )));
                }
                let head = match bundle_head(&heads, branch) {
                    Ok(head) => head,
                    Err(why) => return Ok(Err(why)),
                };
                if guarded.ref_exists(&reference)?.as_ref() != Some(&head) {
                    let (status, stdout, stderr) = capture(
                        guarded
                            .git_root()
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .args([
                                "-c",
                                "transfer.fsckObjects=true",
                                "fetch",
                                "--quiet",
                                "--no-tags",
                                "--no-write-fetch-head",
                                "--no-recurse-submodules",
                            ])
                            .arg(&scratch.0)
                            .arg(format!("+{reference}:{reference}")),
                        BUNDLE_TIMEOUT,
                    )?;
                    if !status.success() {
                        return Ok(Err(format!(
                            "the run bundle ({RUN_BUNDLE_FILE}) cannot be imported: {}",
                            format!("{stdout}{stderr}").trim()
                        )));
                    }
                }
            }
        }
        match guarded.ref_exists(&reference)? {
            Some(head) => Ok(Ok(CloneFacts { head, state })),
            None => Ok(Err(format!(
                "the control side has no run branch {branch} to import into"
            ))),
        }
    }

    /// See [`Repository::landing_worktree`].
    pub fn landing_worktree(
        &self,
        run: &TaskRun,
        commit: &CommitSha,
    ) -> Result<std::result::Result<PathBuf, String>> {
        let path = run
            .landing_path()
            .context("the run has no landing worktree")?;
        let landing_dir = path.parent().context("no landing directory")?;
        let queue_dir = landing_dir.parent().context("no queue directory")?;
        ensure!(
            queue_dir.is_dir(),
            "the queue's directory {} is gone",
            queue_dir.display()
        );
        match entry(landing_dir)? {
            None => DirBuilder::new()
                .mode(0o700)
                .create(landing_dir)
                .with_context(|| format!("create {}", landing_dir.display()))?,
            Some(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Ok(Err(format!(
                    "{} is a symlink or no directory, so no landing worktree is made in it",
                    landing_dir.display()
                )));
            }
            // SAFETY: geteuid reads the process's own effective user ID.
            Some(meta) if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 => {
                return Ok(Err(format!(
                    "{} is not the control side's alone (another owner, or writable by its group or others), so no landing worktree is made in it",
                    landing_dir.display()
                )));
            }
            Some(_) => (),
        }
        let guarded = self.guarded_repository()?;
        let registered = guarded
            .worktrees()?
            .iter()
            .any(|(listed, _)| same_path(listed, &path));
        match entry(&path)? {
            Some(meta) if registered && meta.is_dir() && !meta.file_type().is_symlink() => {
                return Ok(Ok(path));
            }
            Some(_) => {
                return Ok(Err(format!(
                    "{} is there already and is not this run's landing worktree, so none is made there",
                    path.display()
                )));
            }
            None => (),
        }
        output(
            guarded
                .git_root()
                .args(["worktree", "add", "--quiet", "--no-checkout", "--detach"])
                .arg(&path)
                .arg(commit.as_str()),
        )?;
        // A configuration the landing worktree reads beyond the root's (a
        // conditional include) is guarded before its files are written.
        guarded.widen_guard(&path)?;
        output(guarded.command().arg("-C").arg(&path).args([
            "reset",
            "--quiet",
            "--hard",
            commit.as_str(),
        ]))?;
        match entry(&path)? {
            Some(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(Ok(path)),
            _ => Ok(Err(format!(
                "{} is no longer a directory after it was made",
                path.display()
            ))),
        }
    }

    /// See [`Repository::remove_landing_worktree`].
    pub fn remove_landing_worktree(&self, run: &TaskRun) -> Result<()> {
        let Some(path) = run.landing_path() else {
            return Ok(());
        };
        let guarded = self.guarded_repository();
        self.remove_landing_at(guarded.as_ref().ok(), &path)?;
        let guarded = unguarded_left(guarded, &format!("the registration of {}", path.display()))?;
        guarded.prune_worktrees()
    }

    /// Remove the landing worktree at `path`: its registration first with
    /// `guarded`, the guarded repository, when the guard holds, then what
    /// is left there as files, without following a symlink (ADR-t2179-1
    /// decisions 7 and 8). Pruning a registration left is the caller's.
    fn remove_landing_at(&self, guarded: Option<&Self>, path: &Path) -> Result<()> {
        // Nothing is removed through a landing directory that is not one.
        if let Some(meta) = entry(path.parent().context("no landing directory")?)?
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            bail!(
                "{} is a symlink or no directory; nothing is removed through it",
                path.parent().unwrap_or(path).display()
            );
        }
        if let Some(guarded) = guarded {
            let registered = guarded
                .worktrees()?
                .iter()
                .any(|(listed, _)| same_path(listed, path));
            if registered {
                let primary = guarded.primary_worktree()?;
                let _ = output(&mut guarded.remove_worktree_command(&primary, path));
            }
        }
        remove_entry(path)
    }

    /// See [`Repository::hand_main`].
    pub fn hand_main(&self, run: &TaskRun, main: &CommitSha) -> Result<()> {
        let run_dir = Path::new(run.run_dir().context("missing run directory")?);
        let file = run_dir.join(MAIN_BUNDLE_FILE);
        if main == run.base_commit() {
            return remove_entry(&file);
        }
        let guarded = self.guarded_repository()?;
        let branch = guarded.landing_branch()?;
        let (status, bytes, stderr) = capture_bytes(
            guarded.git_root().args([
                "bundle",
                "create",
                "--quiet",
                "-",
                &branch.reference(),
                &format!("^{}", run.base_commit()),
            ]),
            BUNDLE_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "git bundle create of {} failed ({status}): {stderr}",
            branch.name
        );
        write_run_file(&file, &bytes)
    }

    /// See [`Repository::remove_clone`].
    pub fn remove_clone(&self, clone: &Path, branch: &str) -> Result<()> {
        let (Some(landing), Some(run_dir)) = (landing_of(clone), run_dir_of(clone)) else {
            bail!("{} is no run's clone", clone.display());
        };
        let guarded = self.guarded_repository();
        self.remove_landing_at(guarded.as_ref().ok(), &landing)?;
        remove_entry(clone)?;
        for name in [RUN_BUNDLE_FILE, RUN_STATE_FILE, MAIN_BUNDLE_FILE] {
            remove_entry(&run_dir.join(name))?;
        }
        let guarded = unguarded_left(
            guarded,
            &format!(
                "the run branch {branch} and the registration of {}",
                landing.display()
            ),
        )?;
        guarded.prune_worktrees()?;
        guarded.delete_branch(branch)
    }
}

/// The guarded repository, or the error that says the guard does not hold
/// and that `left`, which only the main repository's git removes, is left
/// for a later cleanup: no git runs on it unguarded (ADR-t2179-1 decision
/// 8).
fn unguarded_left(guarded: Result<GitRepository>, left: &str) -> Result<GitRepository> {
    guarded.with_context(|| {
        format!("{left} is left for a later cleanup, since the guard of the control side's git does not hold")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_file_is_read_and_written_only_as_a_regular_file() {
        let dir = std::env::temp_dir().join(format!("dagq-run-files-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, b"host").unwrap();
        let file = dir.join("main.bundle");
        assert_eq!(read_run_file(&file), Ok(None));
        std::os::unix::fs::symlink(&outside, &file).unwrap();
        assert!(read_run_file(&file).unwrap_err().contains("symlink"));
        // Writing replaces the symlink and leaves what it pointed at.
        write_run_file(&file, b"bundle").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"host");
        assert!(
            !fs::symlink_metadata(&file)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_run_file(&file), Ok(Some(b"bundle".to_vec())));
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(
            read_run_file(&file)
                .unwrap_err()
                .contains("not a regular file")
        );
        write_run_file(&file, b"again").unwrap();
        assert_eq!(read_run_file(&file), Ok(Some(b"again".to_vec())));
        // Removing a symlink to a directory leaves the directory.
        let kept = dir.join("kept");
        fs::create_dir(&kept).unwrap();
        fs::write(kept.join("a"), b"a").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        remove_entry(&link).unwrap();
        assert!(kept.join("a").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_control_side_runs_no_git_in_a_clone() {
        assert!(refuse_clone(Path::new("/q/clones/r1")).is_err());
        assert!(refuse_clone(Path::new("/q/runs/r1/worktree")).is_ok());
    }
}
