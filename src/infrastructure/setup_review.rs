//! The host of `setup-review` ([`crate::application::setup_review`]): the
//! main checkout's Git, the temporary place under `<queue
//! dir>/setup-review/<id>/` (a worktree of the checkout at its HEAD in
//! `tree`, the job's output in `job`), the job's agent, the runtime's
//! reading of `dagq.toml`, the person's terminal, and the checkout's
//! working tree the approved files are written to.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, OsString},
    fs,
    io::{IsTerminal, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::ffi::{OsStrExt, OsStringExt},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{
    application::{
        AgentProvider,
        actor_executor::WorkspaceAccess,
        observer::HeadlessAgent,
        prompt::{CurrentSetup, SETUP_REVIEW_ACCESS},
        setup_review::{Generated, InitRepository, SetupPlace, SetupReviewHost},
    },
    domain::{
        ActorContext,
        actor_model::{ActorLaunch, ModelRole},
        review_builtins::RULES_DIR,
        setup_review::{CONFIG_FILE, Change, ChangeStatus, file_diff, writable},
    },
    infrastructure::{
        adapters::{
            OUTPUT_TIMEOUT, capture_bytes, capture_with_input, main_checkout_of, output,
            output_file,
        },
        git_binary::git_executable,
    },
};

/// The directory of the temporary places under the queue's directory.
pub const PLACES_DIR: &str = "setup-review";

/// Whether the person interrupted the command (SIGINT, SIGTERM or SIGHUP)
/// while [`Interrupt`] was installed.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn interrupted(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// The signals that end the command, caught while it runs so that an
/// interrupt (Ctrl-C at the prompt or while the job runs) stops the job,
/// writes nothing and still removes the temporary place. The handler only
/// sets a flag, without `SA_RESTART`, so a read of the person's answer
/// returns; the previous dispositions come back when it is dropped.
pub struct Interrupt {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

/// The signals [`Interrupt`] catches.
const INTERRUPTS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

impl Interrupt {
    pub fn install() -> Result<Self> {
        INTERRUPTED.store(false, Ordering::SeqCst);
        let mut previous = Vec::new();
        for signal in INTERRUPTS {
            // SAFETY: the handler only stores an atomic; both structs are
            // fully initialized before the call.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = interrupted as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_flags = 0;
                let mut old: libc::sigaction = std::mem::zeroed();
                anyhow::ensure!(
                    libc::sigaction(signal, &action, &mut old) == 0,
                    "install the handler of signal {signal}"
                );
                previous.push((signal, old));
            }
        }
        Ok(Self { previous })
    }

    /// Whether an interrupt arrived.
    pub fn happened() -> bool {
        INTERRUPTED.load(Ordering::SeqCst)
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        for (signal, old) in &self.previous {
            // SAFETY: restoring a disposition sigaction returned.
            unsafe {
                libc::sigaction(*signal, old, std::ptr::null_mut());
            }
        }
    }
}

/// The person's answer, one line from the standard input read with
/// `read(2)` so that an interrupt ends it: `None` when interrupted.
fn read_answer() -> Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        if Interrupt::happened() {
            return Ok(None);
        }
        let mut byte = 0u8;
        // SAFETY: reads at most one byte into a live local.
        let read = unsafe { libc::read(0, (&raw mut byte).cast(), 1) };
        match read {
            1 if byte == b'\n' => break,
            1 => line.push(byte),
            0 => break,
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error).context("read the answer");
                }
            }
        }
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// The mode a file the job wrote is given in the checkout: 0755 when the
/// job made it executable by anyone (a program script), else 0644. Only
/// that bit is taken from it, never its other bits (set-user-ID, sticky,
/// the group's and others' write).
fn written_mode(generated: u32) -> u32 {
    if generated & 0o111 == 0 { 0o644 } else { 0o755 }
}

/// Refuse `path` under `checkout` when a part of it under the checkout is
/// a symbolic link or is not a directory on the way.
fn check_target(checkout: &Path, path: &str) -> Result<()> {
    let mut at = checkout.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        anyhow::ensure!(
            !part.is_empty() && *part != "." && *part != "..",
            "{path} is not a plain path under the checkout, so nothing is applied"
        );
        at.push(part);
        let last = index + 1 == parts.len();
        match fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                "{} is a symbolic link in the checkout, so nothing is applied: the write would \
                 go through it",
                at.display()
            ),
            Ok(meta) if !last && !meta.is_dir() => anyhow::bail!(
                "{} is not a directory in the checkout, so nothing is applied",
                at.display()
            ),
            Ok(meta) if last && !meta.is_file() => anyhow::bail!(
                "{} is not a regular file in the checkout, so nothing is applied",
                at.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("read {}", at.display())),
        }
    }
    Ok(())
}

/// What the job starts with: `[roles.setup_review]` of `checkout`'s
/// `dagq.toml`; none, or a file that cannot be read, starts it with the
/// provider's default.
pub fn setup_launch(checkout: &Path) -> ActorLaunch {
    match crate::infrastructure::run_env::load_role_models(checkout) {
        Ok(models) => models.launch(ModelRole::SetupReview),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "[roles.setup_review] could not be read; starting it with the default: {error:#}");
            ActorLaunch::default_of(ModelRole::SetupReview)
        }
    }
}

/// The local host of `setup-review` run in `cwd` on the queue at `db`.
pub struct LocalSetupReview<'a> {
    db: &'a Path,
    cwd: &'a Path,
    provider: &'a dyn AgentProvider,
    launch: &'a ActorLaunch,
    /// The `dagq` binary; its directory goes first on the job's PATH.
    dagq: &'a Path,
    timeout: Duration,
    /// Where the job's agent's events go (the queue through the service,
    /// or the command's own queue); `None` leaves them to the log.
    events: Option<&'a dyn crate::application::RunLog>,
    /// The tree of each place as it was made, by the place's id: what the
    /// job's changes are told from.
    baselines: RefCell<BTreeMap<String, Snapshot>>,
}

impl<'a> LocalSetupReview<'a> {
    pub fn new(
        db: &'a Path,
        cwd: &'a Path,
        provider: &'a dyn AgentProvider,
        launch: &'a ActorLaunch,
        dagq: &'a Path,
        timeout: Duration,
    ) -> Self {
        Self {
            db,
            cwd,
            provider,
            launch,
            dagq,
            timeout,
            events: None,
            baselines: RefCell::new(BTreeMap::new()),
        }
    }

    /// This host with its job's agent's events recorded on `events`.
    #[must_use]
    pub fn with_events(mut self, events: &'a dyn crate::application::RunLog) -> Self {
        self.events = Some(events);
        self
    }
}

/// One entry of a place's tree as its bytes are: a file with whether it
/// is executable, the digest of its content and, at a path the job may
/// write ([`writable`]), the content itself (what is checked, shown and
/// written, never read again); a symbolic link with its target; or
/// anything else (a FIFO, a socket).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    File {
        executable: bool,
        digest: [u8; 32],
        bytes: Option<Vec<u8>>,
    },
    Link(PathBuf),
    Other,
}

/// `openat(2)` of `name` in the directory open as `at` with `flags`,
/// never following a symbolic link at `name` (`O_NOFOLLOW`): a link there
/// is an error. The descriptor is closed on exec.
fn open_at(at: RawFd, name: &OsStr, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: a valid C string; the descriptor returned is owned below.
    let fd = unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Clear `errno`, so that a `readdir(3)` that returns no entry tells its
/// end from its error.
fn clear_errno() {
    // SAFETY: the thread's own errno.
    unsafe {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        {
            *libc::__errno_location() = 0;
        }
    }
}

/// The names in the directory open as `dir`, without `.` and `..`, read
/// from that descriptor: a directory swapped for a link after it was
/// opened is not listed through it. A read that fails is an error, never a
/// shorter list.
fn names_in(dir: &OwnedFd) -> std::io::Result<Vec<OsString>> {
    // fdopendir(3) takes its descriptor over: a duplicate is handed to it.
    let copy = dir.try_clone()?;
    // SAFETY: a valid descriptor, owned by the stream from here on.
    let stream = unsafe { libc::fdopendir(copy.as_raw_fd()) };
    if stream.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    std::mem::forget(copy);
    let mut names = Vec::new();
    let result = loop {
        clear_errno();
        // SAFETY: a stream fdopendir returned and not closed yet.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            break match error.raw_os_error() {
                Some(0) | None => Ok(names),
                Some(_) => Err(error),
            };
        }
        // SAFETY: readdir's entry stays valid until the next call on the
        // stream; its name is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    };
    // SAFETY: the stream is closed once, with its descriptor.
    unsafe { libc::closedir(stream) };
    result
}

/// What `name` in the directory open as `dir` is, by `fstatat(2)` without
/// following a link: its `st_mode`.
fn kind_at(dir: &OwnedFd, name: &OsStr) -> std::io::Result<libc::mode_t> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: a zeroed stat is a valid out parameter.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid descriptor, C string and out parameter.
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stat.st_mode)
}

/// The target of the link `name` in the directory open as `dir`.
fn link_at(dir: &OwnedFd, name: &OsStr) -> std::io::Result<PathBuf> {
    let name = CString::new(name.as_bytes())?;
    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        // SAFETY: a valid descriptor and C string, and a buffer of `size`.
        let read = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                size,
            )
        };
        let Ok(read) = usize::try_from(read) else {
            return Err(std::io::Error::last_os_error());
        };
        if read < size {
            buffer.truncate(read);
            return Ok(PathBuf::from(OsString::from_vec(buffer)));
        }
        size *= 2;
    }
}

/// The bytes of the regular file `name` in the directory open as `dir`
/// and whether it is executable ([`written_mode`]), read from one open of
/// it that follows no link and waits on no FIFO: a file swapped for a link
/// or a FIFO after it was listed is an error, not read through.
fn read_regular(dir: &OwnedFd, name: &OsStr) -> Result<(Vec<u8>, bool)> {
    use std::io::Read;
    let mut file = fs::File::from(open_at(
        dir.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_NONBLOCK,
    )?);
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file(),
        "{name:?} is not a regular file, so nothing is applied"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, written_mode(meta.permissions().mode()) == 0o755))
}

/// A place's tree, each entry by its repository-relative path.
type Snapshot = BTreeMap<String, Entry>;

/// `tree` as its bytes are, read from the file system alone: no Git, so
/// no clean, smudge or process filter, no attribute and no setting of the
/// repository's or the job's comes into what is a change. Each directory
/// is opened once with `O_NOFOLLOW` and listed, and its entries opened,
/// from that descriptor ([`open_at`], [`names_in`]), and each file is read
/// once ([`read_regular`]), so no symbolic link is followed, not even one
/// a process swaps in for a directory or a file while the tree is read: a
/// file outside the place is never read. The tree's own `.git` (its
/// pointer to the worktree's Git directory) is an entry like any other, so
/// a job that rewrites it changes a path outside what it may change; a
/// `.git` directory is not gone into. A name that is not UTF-8 is an error.
fn snapshot(tree: &Path) -> Result<Snapshot> {
    use sha2::{Digest, Sha256};
    let mut entries = Snapshot::new();
    let root = open_at(
        libc::AT_FDCWD,
        tree.as_os_str(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
    .with_context(|| format!("open {}", tree.display()))?;
    let mut dirs = vec![(root, String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        let names = names_in(&dir).with_context(|| format!("list {}{prefix}", tree.display()))?;
        for name in names {
            let text = name.to_str().with_context(|| {
                format!(
                    "{}/{prefix} has a name that is not UTF-8 ({name:?}), so nothing is applied",
                    tree.display()
                )
            })?;
            let path = format!("{prefix}{text}");
            let read = |what: &str| format!("{what} {}/{path}", tree.display());
            let kind = kind_at(&dir, &name).with_context(|| read("read"))? & libc::S_IFMT;
            let value = match kind {
                // A Git directory is an entry of its own, never gone into:
                // its appearing or going is a change, and its files are
                // Git's, not the job's to write.
                libc::S_IFDIR if text.eq_ignore_ascii_case(".git") => Entry::Other,
                libc::S_IFDIR => {
                    let child = open_at(dir.as_raw_fd(), &name, libc::O_RDONLY | libc::O_DIRECTORY)
                        .with_context(|| read("open"))?;
                    dirs.push((child, format!("{path}/")));
                    continue;
                }
                libc::S_IFLNK => Entry::Link(link_at(&dir, &name).with_context(|| read("read"))?),
                libc::S_IFREG => {
                    let (bytes, executable) =
                        read_regular(&dir, &name).with_context(|| read("read"))?;
                    Entry::File {
                        executable,
                        digest: Sha256::digest(&bytes).into(),
                        bytes: writable(&path).then_some(bytes),
                    }
                }
                _ => Entry::Other,
            };
            entries.insert(path, value);
        }
    }
    Ok(entries)
}

/// What changed from `before` to `after`, in the order of the paths.
fn changed(before: &Snapshot, after: &Snapshot) -> Vec<Change> {
    let paths: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let status = match (before.get(path), after.get(path)) {
                (None, Some(_)) => ChangeStatus::Added,
                (Some(_), None) => ChangeStatus::Deleted,
                (Some(was), Some(is)) if was != is => ChangeStatus::Modified,
                _ => return None,
            };
            Some(Change {
                path: path.clone(),
                status,
            })
        })
        .collect()
}

/// Git in `dir` with no hook (`core.hooksPath=/dev/null`) and no
/// fsmonitor of the repository's run, whatever it configures: making and
/// removing the place moves refs and worktrees, which run hooks otherwise.
/// A path is a path, never a pattern (`--literal-pathspecs`).
fn git(dir: &Path) -> Result<Command> {
    let mut command = Command::new(git_executable()?);
    command.arg("-C").arg(dir).args([
        "--literal-pathspecs",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    Ok(command)
}

/// How long writing the place's files from the commit's objects may take:
/// a repository's whole tree is read.
const PLACE_TIMEOUT: Duration = Duration::from_secs(300);

/// One entry of a commit's tree as `git ls-tree -r -z --full-tree` lists
/// it: what the place is written from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    /// A file: its object and whether its mode is executable.
    File {
        path: String,
        oid: String,
        executable: bool,
    },
    /// A symbolic link, whose object is its target.
    Link { path: String, oid: String },
    /// A submodule's commit: an empty directory, as a checkout leaves it.
    Submodule { path: String },
}

/// The entries of `listing`, the output of `ls-tree -r -z` (each
/// `<mode> <type> <object>\t<path>` ended by NUL). A path that is not
/// UTF-8, or has an empty, `.`, `..` or `.git` part (in any case), is an
/// error: Git checks none of them out, and one could reach outside the
/// place.
fn tree_entries(listing: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    for record in listing.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(record)
            .map_err(|_| anyhow::anyhow!("a path of the commit is not UTF-8"))?;
        let (meta, path) = text
            .split_once('\t')
            .with_context(|| format!("read the tree's entry {text:?}"))?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some(_), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
            anyhow::bail!("read the tree's entry {text:?}");
        };
        anyhow::ensure!(
            path.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.eq_ignore_ascii_case(".git")
            }),
            "{path} of the commit is not a path a checkout writes"
        );
        let (path, oid) = (path.to_owned(), oid.to_owned());
        entries.push(match mode {
            "100644" | "100755" => TreeEntry::File {
                path,
                oid,
                executable: mode == "100755",
            },
            "120000" => TreeEntry::Link { path, oid },
            "160000" => TreeEntry::Submodule { path },
            other => anyhow::bail!("{path} of the commit has the mode {other}"),
        });
    }
    Ok(entries)
}

/// The contents `cat-file --batch` printed in `printed` for `count`
/// objects asked for in order: each `<object> <type> <size>\n`, the
/// content and a line break. A missing object is an error.
fn batch_contents(printed: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    let mut contents = Vec::with_capacity(count);
    let mut at = 0;
    for _ in 0..count {
        let end = printed[at..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|found| at + found)
            .context("git cat-file --batch printed fewer objects than asked")?;
        let header = String::from_utf8_lossy(&printed[at..end]);
        let size: usize = header
            .rsplit(' ')
            .next()
            .and_then(|size| size.parse().ok())
            .with_context(|| format!("read the object {header}"))?;
        let start = end + 1;
        anyhow::ensure!(
            start + size <= printed.len(),
            "git cat-file --batch printed the object {header} short"
        );
        contents.push(printed[start..start + size].to_vec());
        at = start + size + 1;
    }
    Ok(contents)
}

/// Write `bytes` to `path` under `root`, 0755 when `executable` and 0644
/// otherwise: [`check_target`] before and after its directories are made,
/// and the file opened with `O_NOFOLLOW`, so no symbolic link under the
/// root is followed.
fn write_under(root: &Path, path: &str, bytes: &[u8], executable: bool) -> Result<()> {
    check_target(root, path)?;
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    check_target(root, path)?;
    let mode = if executable { 0o755 } else { 0o644 };
    let mut written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&target)
        .with_context(|| format!("write {}", target.display()))?;
    written
        .write_all(bytes)
        .with_context(|| format!("write {}", target.display()))?;
    written
        .set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("set the mode of {}", target.display()))
}

/// Write `commit`'s files into `tree` from its objects, read by
/// `checkout`'s Git (`ls-tree`, `cat-file --batch`), as a checkout would
/// but running no filter, conversion or hook of the repository's: a file
/// is its object's bytes, executable as its mode says, a link's target is
/// its object, and a submodule is an empty directory. Links are made after
/// every file, and each path is refused when a link is on its way
/// ([`check_target`]), so nothing is written outside the tree.
fn materialize(checkout: &Path, commit: &str, tree: &Path) -> Result<()> {
    let (status, listing, error) = capture_bytes(
        git(checkout)?.args(["ls-tree", "-r", "-z", "--full-tree", commit]),
        PLACE_TIMEOUT,
    )?;
    anyhow::ensure!(status.success(), "git ls-tree {commit} failed: {error}");
    let entries = tree_entries(&listing)?;
    let mut input = output_file()?;
    let mut asked = 0;
    for entry in &entries {
        if let TreeEntry::File { oid, .. } | TreeEntry::Link { oid, .. } = entry {
            writeln!(input, "{oid}")?;
            asked += 1;
        }
    }
    input.seek(SeekFrom::Start(0))?;
    let (status, printed, error) = capture_with_input(
        git(checkout)?.args(["cat-file", "--batch"]),
        input,
        PLACE_TIMEOUT,
    )?;
    anyhow::ensure!(status.success(), "git cat-file --batch failed: {error}");
    let mut contents = batch_contents(&printed, asked)?.into_iter();
    let mut links = Vec::new();
    for entry in entries {
        match entry {
            TreeEntry::File {
                path, executable, ..
            } => {
                let bytes = contents.next().context("an object was not read")?;
                write_under(tree, &path, &bytes, executable)?;
            }
            TreeEntry::Link { path, .. } => {
                links.push((path, contents.next().context("an object was not read")?));
            }
            TreeEntry::Submodule { path } => {
                check_target(tree, &path)?;
                fs::create_dir_all(tree.join(&path))
                    .with_context(|| format!("create {}", tree.join(&path).display()))?;
            }
        }
    }
    for (path, target) in links {
        check_target(tree, &path)?;
        let link = tree.join(&path);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        check_target(tree, &path)?;
        std::os::unix::fs::symlink(OsStr::from_bytes(&target), &link)
            .with_context(|| format!("link {}", link.display()))?;
    }
    Ok(())
}

/// Git on the place's objects after its job wrote in its tree: the
/// worktree's own Git directory, read when the place was made and outside
/// the tree, is named, so a `.git` the job rewrote in the tree is not
/// followed; it runs outside the tree and reads objects only, never the
/// tree's files, so no filter runs.
fn place_git(place: &SetupPlace) -> Result<Command> {
    let mut command = Command::new(git_executable()?);
    command
        .arg("--git-dir")
        .arg(&place.git_dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .current_dir(&place.dir);
    Ok(command)
}

/// The bytes of `path` in `commit`, as Git keeps them (`cat-file blob`,
/// which runs no filter or conversion), `None` when the commit has none.
fn committed_bytes(mut git: Command, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let (status, bytes, _) = capture_bytes(
        git.args(["cat-file", "blob"])
            .arg(format!("{commit}:{path}")),
        OUTPUT_TIMEOUT,
    )?;
    Ok(status.success().then_some(bytes))
}

/// The `(mode, object)` of each entry `listing` has, the output of
/// `git ls-tree -z` (`<mode> <type> <object>\t<path>`) when `staged` is
/// false and of `git ls-files -s -z` (`<mode> <object> <stage>\t<path>`)
/// when it is true.
fn listed_entries(listing: &str, staged: bool) -> Vec<(String, String)> {
    listing
        .split('\0')
        .filter_map(|record| record.split_once('\t'))
        .filter_map(|(meta, _)| {
            let fields: Vec<&str> = meta.split(' ').collect();
            let oid = if staged { fields.get(1) } else { fields.get(2) };
            Some((fields.first()?.to_string(), oid?.to_string()))
        })
        .collect()
}

/// What the checkout's working tree has at a path, read from the file
/// system alone (no filter, and no flag of the index, such as
/// assume-unchanged or skip-worktree, comes into it).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Working {
    Absent,
    File {
        bytes: Vec<u8>,
        executable: bool,
    },
    /// A directory, a link or anything else but a regular file.
    Other,
}

/// What HEAD has at a path: its mode and object, and the object's bytes
/// when it is a file (`100644`, `100755`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Committed {
    mode: String,
    oid: String,
    bytes: Option<Vec<u8>>,
}

/// Whether the person has a change not committed at a path the
/// generation writes, which the write would lose: the index's entries
/// differ from HEAD's (a change staged, a path added or removed, a
/// conflict), the working tree has a file where HEAD has none (untracked
/// or ignored), has none where HEAD has one, or has bytes or an executable
/// bit other than HEAD's. The bytes are compared as they are, with no
/// filter, so a file a filter or a line ending conversion of the
/// repository changes counts as changed.
fn dirty(head: Option<&Committed>, index: &[(String, String)], working: &Working) -> bool {
    let staged = match head {
        Some(head) => index != [(head.mode.clone(), head.oid.clone())],
        None => !index.is_empty(),
    };
    staged
        || match (head, working) {
            (None, Working::Absent) => false,
            (None, _) | (Some(_), Working::Absent | Working::Other) => true,
            (Some(head), Working::File { bytes, executable }) => {
                head.bytes.as_deref() != Some(bytes.as_slice())
                    || (head.mode == "100755") != *executable
            }
        }
}

impl LocalSetupReview<'_> {
    /// `path` of `commit` in `checkout`, `None` when the commit has none.
    fn committed(checkout: &Path, commit: &str, path: &str) -> Result<Option<String>> {
        let listed = output(git(checkout)?.args(["ls-tree", "--name-only", commit, "--", path]))?;
        if listed.trim().is_empty() {
            return Ok(None);
        }
        let bytes = committed_bytes(git(checkout)?, commit, path)?
            .with_context(|| format!("read {path} of {commit}"))?;
        String::from_utf8(bytes)
            .map(Some)
            .with_context(|| format!("{path} of {commit} is not UTF-8"))
    }

    fn make(&self, checkout: &Path, id: &str, root: &Path) -> Result<SetupPlace> {
        let tree = root.join("tree");
        let dir = root.join("job");
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let commit = self.head(checkout)?;
        // No checkout by Git: its filters and hooks would run on the
        // repository's say before anything is approved.
        output(
            git(checkout)?
                .args(["worktree", "add", "--quiet", "--detach", "--no-checkout"])
                .arg(&tree)
                .arg(&commit),
        )?;
        let git_dir =
            PathBuf::from(output(git(&tree)?.args(["rev-parse", "--absolute-git-dir"]))?.trim());
        materialize(checkout, &commit, &tree)?;
        // The index as the commit has it, from its objects alone, so Git in
        // the place sees the files as they were committed.
        output(
            Command::new(git_executable()?)
                .arg("--git-dir")
                .arg(&git_dir)
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "read-tree",
                ])
                .arg(&commit)
                .current_dir(&dir),
        )?;
        self.baselines
            .borrow_mut()
            .insert(id.to_owned(), snapshot(&tree)?);
        Ok(SetupPlace {
            id: id.to_owned(),
            tree,
            dir,
            commit,
            git_dir,
        })
    }
}

/// The repository `dagq init` runs in, at `cwd`: its main checkout, the
/// `dagq.toml` of its working tree and its files as Git lists them.
pub struct LocalInitRepository<'a> {
    pub cwd: &'a Path,
}

impl InitRepository for LocalInitRepository<'_> {
    fn checkout(&self) -> Result<PathBuf> {
        main_checkout_of(self.cwd)
    }

    fn declarations(
        &self,
        checkout: &Path,
    ) -> Result<Vec<crate::domain::toolchain::ToolchainDeclaration>> {
        crate::infrastructure::run_env::load_toolchain_declarations(checkout)
    }

    fn files(&self, checkout: &Path) -> Result<Vec<String>> {
        crate::infrastructure::adapters::checkout_files(checkout)
    }
}

impl SetupReviewHost for LocalSetupReview<'_> {
    fn checkout(&self) -> Result<PathBuf> {
        main_checkout_of(self.cwd).context("setup-review runs in a repository's checkout")
    }

    fn current(&self, checkout: &Path, commit: &str) -> Result<CurrentSetup> {
        let listed = output(git(checkout)?.args([
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            commit,
            "--",
            &format!("{RULES_DIR}/"),
        ]))?;
        let mut files = Vec::new();
        for path in listed.split('\0').filter(|path| !path.is_empty()) {
            if let Some(text) = Self::committed(checkout, commit, path)? {
                files.push((path.to_owned(), text));
            }
        }
        Ok(CurrentSetup {
            config: Self::committed(checkout, commit, CONFIG_FILE)?,
            files,
        })
    }

    fn head(&self, checkout: &Path) -> Result<String> {
        Ok(output(git(checkout)?.args(["rev-parse", "HEAD"]))?
            .trim()
            .to_owned())
    }

    /// A place that could not be made whole is removed before the error.
    fn prepare(&self, checkout: &Path) -> Result<SetupPlace> {
        let queue_dir = self.db.parent().context("the queue has no directory")?;
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let root = queue_dir.join(PLACES_DIR).join(&id);
        self.make(checkout, &id, &root).inspect_err(|_| {
            let _ = fs::remove_dir_all(&root);
            let _ = git(checkout).map(|mut prune| output(prune.args(["worktree", "prune"])));
        })
    }

    fn run(&self, place: &SetupPlace, prompt: &str, actor: &ActorContext) -> Result<Option<i32>> {
        fs::write(place.dir.join("prompt.txt"), prompt)
            .with_context(|| format!("write the prompt in {}", place.dir.display()))?;
        crate::infrastructure::observer::run_agent_in(
            self.provider,
            self.db,
            self.events,
            WorkspaceAccess::Write(place.tree.clone()),
            &place.dir,
            prompt,
            &HeadlessAgent {
                actor: actor.clone(),
                session_id: None,
                launch: self.launch,
                dagq: self.dagq,
                timeout: self.timeout,
                what: "the generation of the review's settings",
                access: SETUP_REVIEW_ACCESS,
            },
            &Interrupt::happened,
        )
    }

    /// The job's changes are the differences of the tree's bytes from
    /// what they were when the place was made ([`snapshot`]), each file
    /// with the bytes that snapshot read, and each file's diff is made from
    /// those bytes, against the commit's (`cat-file blob`): no filter,
    /// attribute, external diff or text conversion of the repository's
    /// settings or of the job's files decides what the person sees, and
    /// the place is not read again for the checks or the write.
    fn changes(&self, place: &SetupPlace) -> Result<(Vec<Generated>, String)> {
        let baselines = self.baselines.borrow();
        let before = baselines
            .get(&place.id)
            .context("the place's tree was not read when it was made")?;
        let after = snapshot(&place.tree)?;
        let mut files = Vec::new();
        let mut diff = String::new();
        for change in changed(before, &after) {
            let content = match after.get(&change.path) {
                None => None,
                Some(Entry::File {
                    executable, bytes, ..
                }) => bytes.clone().map(|bytes| (bytes, *executable)),
                Some(_) => anyhow::bail!(
                    "{} is not a regular file (a symbolic link or a directory), so nothing is \
                     applied",
                    place.tree.join(&change.path).display()
                ),
            };
            if writable(&change.path) {
                let old =
                    committed_bytes(place_git(place)?, &place.commit, &change.path)?.map(|bytes| {
                        let executable = matches!(
                            before.get(&change.path),
                            Some(Entry::File {
                                executable: true,
                                ..
                            })
                        );
                        (bytes, executable)
                    });
                diff.push_str(&file_diff(
                    &change.path,
                    old.as_ref()
                        .map(|(bytes, executable)| (bytes.as_slice(), *executable)),
                    content
                        .as_ref()
                        .map(|(bytes, executable)| (bytes.as_slice(), *executable)),
                ));
            }
            files.push(Generated { change, content });
        }
        Ok((files, diff))
    }

    fn check_config(&self, text: &str) -> Result<()> {
        crate::infrastructure::run_env::parse_config(text).map(|_| ())
    }

    fn toolchains(
        &self,
        text: &str,
    ) -> Result<Vec<crate::domain::toolchain::ToolchainDeclaration>> {
        crate::infrastructure::run_env::parse_config(text).map(|config| config.toolchains)
    }

    /// `ls-tree` of the commit's objects: no file of the place is read.
    fn files(&self, checkout: &Path, commit: &str) -> Result<Vec<String>> {
        let listed = output(git(checkout)?.args([
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            "--full-tree",
            commit,
        ]))?;
        Ok(listed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Each of `paths` itself, never a directory above it ([`dirty`]):
    /// HEAD's entry (`ls-tree`) and its bytes (`cat-file`), the index's
    /// entries (`ls-files -s`) and the working tree's file read from the
    /// file system. No `git status`, which would run the repository's
    /// clean and process filters on a changed file and trust the index's
    /// assume-unchanged and skip-worktree flags. An ignored directory that
    /// holds other files does not make a path under it dirty.
    fn uncommitted(&self, checkout: &Path, paths: &[String]) -> Result<Vec<String>> {
        let mut dirty_paths = Vec::new();
        for path in paths {
            let listed = output(git(checkout)?.args(["ls-tree", "-z", "HEAD", "--", path]))?;
            let head = match listed_entries(&listed, false).into_iter().next() {
                Some((mode, oid)) => {
                    let bytes = if matches!(mode.as_str(), "100644" | "100755") {
                        let (status, bytes, error) = capture_bytes(
                            git(checkout)?.args(["cat-file", "blob", &oid]),
                            OUTPUT_TIMEOUT,
                        )?;
                        anyhow::ensure!(status.success(), "git cat-file {oid} failed: {error}");
                        Some(bytes)
                    } else {
                        None
                    };
                    Some(Committed { mode, oid, bytes })
                }
                None => None,
            };
            let index = listed_entries(
                &output(git(checkout)?.args(["ls-files", "-s", "-z", "--", path]))?,
                true,
            );
            let file = checkout.join(path);
            let working = match fs::symlink_metadata(&file) {
                Ok(meta) if meta.is_file() => Working::File {
                    bytes: fs::read(&file).with_context(|| format!("read {}", file.display()))?,
                    executable: meta.permissions().mode() & 0o111 != 0,
                },
                Ok(_) => Working::Other,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Working::Absent,
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", file.display()));
                }
            };
            if dirty(head.as_ref(), &index, &working) {
                dirty_paths.push(path.clone());
            }
        }
        Ok(dirty_paths)
    }

    fn approve(&self, checkout: &Path, diff: &str) -> Result<bool> {
        let mut stderr = std::io::stderr();
        writeln!(stderr, "{diff}")?;
        if !std::io::stdin().is_terminal() {
            writeln!(
                stderr,
                "No terminal to ask on: nothing is applied. Run it in a terminal, or pass --yes \
                 to approve beforehand."
            )?;
            return Ok(false);
        }
        write!(
            stderr,
            "Write these changes to the working tree of {}? Nothing is committed. [y/N] ",
            checkout.display()
        )?;
        stderr.flush()?;
        let Some(answer) = read_answer()? else {
            writeln!(stderr, "\nInterrupted: nothing is applied.")?;
            return Ok(false);
        };
        Ok(!Interrupt::happened()
            && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    /// An interrupt before the writes stops them too.
    fn check_target(&self, checkout: &Path, path: &str) -> Result<()> {
        anyhow::ensure!(!Interrupt::happened(), "interrupted: nothing is applied");
        check_target(checkout, path)
    }

    /// Checked here and by [`write_under`] ([`check_target`]), so no
    /// symbolic link of the checkout is followed. The bytes are those read
    /// after the job ([`Generated`]), never the place's file again.
    fn apply(&self, checkout: &Path, file: &Generated) -> Result<()> {
        let path = &file.change.path;
        check_target(checkout, path)?;
        let target = checkout.join(path);
        let Some((bytes, executable)) = &file.content else {
            anyhow::ensure!(
                file.change.status == ChangeStatus::Deleted,
                "{path} was not read after the job, so nothing is applied"
            );
            return fs::remove_file(&target)
                .with_context(|| format!("remove {}", target.display()));
        };
        write_under(checkout, path, bytes, *executable)
    }

    /// The worktree, its directory and Git's record of it: a worktree Git
    /// could not remove is removed with its directory and then pruned.
    fn remove(&self, checkout: &Path, place: &SetupPlace) -> Result<()> {
        let _ = output(
            git(checkout)?
                .args(["worktree", "remove", "--force"])
                .arg(&place.tree),
        );
        let root = place.dir.parent().unwrap_or(&place.dir);
        if root.exists() {
            fs::remove_dir_all(root).with_context(|| format!("remove {}", root.display()))?;
        }
        output(git(checkout)?.args(["worktree", "prune"])).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree's bytes alone decide what changed: its `.git` is left out,
    /// a file's content and executable bit count, and a link is a link.
    #[test]
    fn what_changed_is_read_from_the_trees_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path();
        fs::write(tree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        fs::create_dir_all(tree.join(".dagq/review")).unwrap();
        fs::write(tree.join("dagq.toml"), "[run.env]\n").unwrap();
        fs::write(tree.join("README.md"), "# A\n").unwrap();
        let before = snapshot(tree).unwrap();
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            [".git", "README.md", "dagq.toml"]
        );
        // The tree's `.git` is read as it is, never followed: rewriting it
        // is a change.
        fs::write(tree.join(".git"), "gitdir: /the-jobs\n").unwrap();
        fs::write(tree.join("dagq.toml"), "[run.env]\nA = \"1\"\n").unwrap();
        fs::write(tree.join(".dagq/review/builtin-tests.md"), "t\n").unwrap();
        fs::set_permissions(tree.join("README.md"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tree.join(".dagq/review/link.md")).unwrap();
        let after = snapshot(tree).unwrap();
        let changes = changed(&before, &after);
        assert_eq!(
            changes
                .iter()
                .map(|change| (change.path.as_str(), change.status))
                .collect::<Vec<_>>(),
            [
                (".dagq/review/builtin-tests.md", ChangeStatus::Added),
                (".dagq/review/link.md", ChangeStatus::Added),
                (".git", ChangeStatus::Modified),
                ("README.md", ChangeStatus::Modified),
                ("dagq.toml", ChangeStatus::Modified),
            ]
        );
        assert_eq!(
            after[".dagq/review/link.md"],
            Entry::Link(PathBuf::from("/etc/passwd"))
        );
        // A `.git` directory is one entry, not gone into.
        fs::remove_file(tree.join(".git")).unwrap();
        fs::create_dir_all(tree.join(".git/hooks")).unwrap();
        fs::create_dir_all(tree.join(".dagq/review/.git")).unwrap();
        fs::write(tree.join(".dagq/review/.git/config"), "[core]\n").unwrap();
        let again = snapshot(tree).unwrap();
        assert_eq!(again[".git"], Entry::Other);
        assert_eq!(again[".dagq/review/.git"], Entry::Other);
        assert!(!again.keys().any(|path| path.contains(".git/")));
    }

    /// The place is written from the commit's tree as `ls-tree` lists it:
    /// files with their mode, links, submodules as directories; a path a
    /// checkout would not write stops it.
    #[test]
    fn the_commits_tree_is_read_and_a_path_outside_it_refused() {
        let listing = b"100644 blob a1\tREADME.md\x00100755 blob b2\ttools/run.sh\x00\
120000 blob c3\tdocs/link.md\x00160000 commit d4\tvendor/lib\x00";
        assert_eq!(
            tree_entries(listing).unwrap(),
            [
                TreeEntry::File {
                    path: "README.md".into(),
                    oid: "a1".into(),
                    executable: false
                },
                TreeEntry::File {
                    path: "tools/run.sh".into(),
                    oid: "b2".into(),
                    executable: true
                },
                TreeEntry::Link {
                    path: "docs/link.md".into(),
                    oid: "c3".into()
                },
                TreeEntry::Submodule {
                    path: "vendor/lib".into()
                },
            ]
        );
        for outside in [
            &b"100644 blob a1\t../x\x00"[..],
            b"100644 blob a1\ta/./x\x00",
            b"100644 blob a1\t.GIT/hooks/post-checkout\x00",
            b"100644 blob a1\ta//x\x00",
            b"100644 blob a1\t\xff\x00",
        ] {
            assert!(tree_entries(outside).is_err(), "{outside:?}");
        }
    }

    /// `cat-file --batch`'s output gives each object's bytes, a line break
    /// in them included; a missing or short object is an error.
    #[test]
    fn the_objects_are_read_from_the_batch() {
        let printed = b"a1 blob 3\nab\n\nb2 blob 0\n\n";
        assert_eq!(
            batch_contents(printed, 2).unwrap(),
            [b"ab\n".to_vec(), Vec::new()]
        );
        assert!(batch_contents(b"a1 missing\n", 1).is_err());
        assert!(batch_contents(b"a1 blob 9\nab\n", 1).is_err());
        assert!(batch_contents(printed, 3).is_err());
    }

    /// The entries of `ls-tree` and `ls-files -s` give their mode and
    /// object.
    #[test]
    fn the_entries_of_head_and_the_index_are_read() {
        assert!(listed_entries("", false).is_empty());
        assert_eq!(
            listed_entries("100644 blob a1\tdagq.toml\0", false),
            [("100644".to_owned(), "a1".to_owned())]
        );
        assert_eq!(
            listed_entries("100755 b2 0\t.dagq/review/programs/x.sh\0", true),
            [("100755".to_owned(), "b2".to_owned())]
        );
    }

    /// A path is clean only when the index has HEAD's entry and the working
    /// tree HEAD's bytes and executable bit, compared as they are: an edit
    /// the index's flags hide, a change staged, a deletion, a file where
    /// HEAD has none and anything but a regular file are changes.
    #[test]
    fn a_change_not_committed_is_found_from_the_bytes_and_the_index() {
        let head = Committed {
            mode: "100644".into(),
            oid: "a1".into(),
            bytes: Some(b"[run.env]\n".to_vec()),
        };
        let index = [("100644".to_owned(), "a1".to_owned())];
        let file = |bytes: &[u8], executable| Working::File {
            bytes: bytes.to_vec(),
            executable,
        };
        assert!(!dirty(Some(&head), &index, &file(b"[run.env]\n", false)));
        assert!(!dirty(None, &[], &Working::Absent));
        for (head, index, working) in [
            // Edited, also when assume-unchanged or skip-worktree hides it.
            (Some(&head), &index[..], file(b"[run.env]\nA = 1\n", false)),
            (Some(&head), &index[..], file(b"[run.env]\n", true)),
            (Some(&head), &index[..], Working::Absent),
            (Some(&head), &index[..], Working::Other),
            // Staged, with the working tree back as committed.
            (
                Some(&head),
                &[("100644".to_owned(), "b2".to_owned())][..],
                file(b"[run.env]\n", false),
            ),
            (Some(&head), &[][..], file(b"[run.env]\n", false)),
            // Untracked or ignored, or added to the index only.
            (None, &[][..], file(b"mine\n", false)),
            (None, &index[..], Working::Absent),
        ] {
            assert!(
                dirty(head, index, &working),
                "{head:?} {index:?} {working:?}"
            );
        }
    }

    /// The tree is read from descriptors: a link is a link, never opened as
    /// a directory or a file, so what it points to outside is not read.
    #[test]
    fn a_link_in_the_tree_is_never_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let (tree, outside) = (dir.path().join("tree"), dir.path().join("outside"));
        fs::create_dir_all(tree.join(".dagq")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.md"), "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join(".dagq/review")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), tree.join("dagq.toml")).unwrap();
        let read = snapshot(&tree).unwrap();
        assert_eq!(read[".dagq/review"], Entry::Link(outside.clone()));
        assert_eq!(read["dagq.toml"], Entry::Link(outside.join("secret.md")));
        assert!(!read.keys().any(|path| path.contains("secret")));
        let at = open_at(
            libc::AT_FDCWD,
            tree.join(".dagq").as_os_str(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .unwrap();
        assert!(
            open_at(
                at.as_raw_fd(),
                OsStr::new("review"),
                libc::O_RDONLY | libc::O_DIRECTORY
            )
            .is_err()
        );
        let root = open_at(
            libc::AT_FDCWD,
            tree.as_os_str(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .unwrap();
        assert!(read_regular(&root, OsStr::new("dagq.toml")).is_err());
    }

    /// A table hidden in a string that spans lines passes the line-wise
    /// check of the tables, and the runtime's reading, which takes each
    /// value on its one line, refuses it.
    #[test]
    fn a_table_hidden_in_a_string_over_lines_is_not_read() {
        let before = "[run.env]\nA = \"1\"\n";
        let hidden = "[review.programs.x]\nscript = \"\"\"\n[run.env]\nA = \"1\"\n#\"\"\"\n";
        assert_eq!(
            crate::domain::setup_review::check_config(Some(before), Some(hidden)),
            Ok(())
        );
        assert!(crate::infrastructure::run_env::parse_config(hidden).is_err());
    }

    #[test]
    fn a_written_file_takes_only_the_executable_bit() {
        for (generated, written) in [
            (0o644, 0o644),
            (0o666, 0o644),
            (0o600, 0o644),
            (0o755, 0o755),
            (0o4777, 0o755),
            (0o700, 0o755),
            (0o1744, 0o755),
        ] {
            assert_eq!(written_mode(generated), written, "{generated:o}");
        }
    }

    #[test]
    fn a_target_under_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("repo");
        let outside = dir.path().join("outside");
        fs::create_dir_all(checkout.join(".dagq/review")).unwrap();
        fs::create_dir(&outside).unwrap();
        assert!(check_target(&checkout, ".dagq/review/builtin-tests.md").is_ok());
        assert!(check_target(&checkout, "new/dir/file.md").is_ok());
        std::os::unix::fs::symlink(&outside, checkout.join(".dagq/review/programs")).unwrap();
        std::os::unix::fs::symlink(outside.join("f"), checkout.join("dagq.toml")).unwrap();
        for path in [".dagq/review/programs/check.sh", "dagq.toml"] {
            let error = check_target(&checkout, path).unwrap_err().to_string();
            assert!(
                error.contains("is a symbolic link in the checkout"),
                "{error}"
            );
        }
        assert!(check_target(&checkout, ".dagq/../x").is_err());
    }
}
