//! The git backend ([Broker] "git"): `git.status`, `git.diff`, `git.log`,
//! `git.show`, `git.add`, `git.commit` and `git.restore`, on the token's
//! worktree only.
//!
//! It runs the `git` executable in the token's workspace only: the worker
//! never names a repository. Before git runs, the workspace is opened from
//! its mounted root without following a symlink (the fs backend's walk),
//! and its `.git` must be a regular file (a worktree's gitdir pointer),
//! read once here. The gitdir it names must be `<common>/worktrees/<name>`
//! outside every mounted root, lead back to that common dir (`commondir`)
//! and point back to the workspace's `.git` (`gitdir`), so a rewritten
//! `.git` reaches neither the main checkout, nor another run's worktree,
//! nor a repository the worker made in its workspace. Every git process is
//! then given the resolved `GIT_DIR`, `GIT_COMMON_DIR` and `GIT_WORK_TREE`
//! and never reads `.git` again. The common dir is mounted at the same
//! absolute path in the container (ADR-t827-2 decision 5).
//!
//! Git runs with an empty environment but a fixed few (no credential
//! prompt, no system or global config, a fresh `HOME`, the token's
//! committer) and with command-line config that switches off everything in
//! the repository's config that would run a program: hooks, the credential
//! helper, fsmonitor, the pager and editor, the ask-pass program, signing,
//! external diff and textconv, the clean / smudge / process filters it
//! defines, auto gc, and every transport (`protocol.allow=never` and each
//! protocol's own `allow`, `GIT_ALLOW_PROTOCOL` empty). There
//! is no push, fetch, remote, config, checkout, branch or reset operation.
//! `git.show` shows only commits `HEAD` reaches (not another run's branch),
//! and `git.restore` restores the worktree from the index or the index from
//! `HEAD`, never from another revision. `git.commit` builds the commit with plumbing (`write-tree`,
//! `commit-tree`, `update-ref` with the old value) on the token's branch
//! only, and only while `HEAD` is that branch.
//!
//! Every git process has the time of `Limits::exec_timeout_secs` (what it
//! leaves holding its pipes is killed with its process group at the same
//! deadline) and its output is bounded by `Limits::output_limit_bytes`:
//! `git.diff` and `git.show` answer `truncated`, the others `output_limit`.
//!
//! [Broker]: https://github.com/hisamekms/dagq/blob/main/docs/design/broker.md

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dagq_broker_protocol::{ErrorCode, encode, git};

use crate::backend::{Backend, BackendRequest, Call, Done, Failure};
use crate::backends::fs::{lstat_at, open_regular, open_workspace};

/// The default and the most commits `git.log` answers.
pub const LOG_DEFAULT: u32 = 20;
pub const LOG_MAX: u32 = 200;

/// The most bytes of git's stderr kept for the error message.
const STDERR_LIMIT: usize = 64 * 1024;

/// The config every git of the broker runs with, before the filters
/// ([`GitBackend::filter_overrides`]) and `safe.directory`.
const CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "credential.helper=",
    "core.askPass=",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "core.pager=cat",
    "core.editor=false",
    "core.quotePath=false",
    "core.sshCommand=false",
    "core.attributesFile=/dev/null",
    "core.excludesFile=/dev/null",
    "protocol.allow=never",
    "protocol.file.allow=never",
    "protocol.ext.allow=never",
    "protocol.ssh.allow=never",
    "protocol.git.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
    "commit.gpgSign=false",
    "log.showSignature=false",
    "diff.ignoreSubmodules=all",
    "color.ui=false",
    "gc.auto=0",
    "maintenance.auto=false",
];

/// The git backend over the server's mounted roots.
#[derive(Debug, Clone)]
pub struct GitBackend {
    roots: Vec<PathBuf>,
    program: PathBuf,
    path: OsString,
}

impl GitBackend {
    /// A backend for workspaces under `roots` (`--root`) that runs `git`
    /// from the server's own `PATH`.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self::with_program(roots, PathBuf::from("git"))
    }

    /// The same with the git executable `program`.
    pub fn with_program(roots: Vec<PathBuf>, program: PathBuf) -> Self {
        Self {
            roots,
            program,
            path: std::env::var_os("PATH")
                .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin")),
        }
    }
}

impl Backend for GitBackend {
    fn call(&self, call: &Call<'_>, request: BackendRequest) -> Result<Done, Failure> {
        let body = match request {
            BackendRequest::GitStatus(_)
            | BackendRequest::GitDiff(_)
            | BackendRequest::GitLog(_)
            | BackendRequest::GitShow(_)
            | BackendRequest::GitAdd(_)
            | BackendRequest::GitCommit(_)
            | BackendRequest::GitRestore(_) => {
                let repo = self.open(call)?;
                let result = match request {
                    BackendRequest::GitStatus(_) => status(&repo),
                    BackendRequest::GitDiff(request) => diff(&repo, &request, &call.confined),
                    BackendRequest::GitLog(request) => log(&repo, &request),
                    BackendRequest::GitShow(request) => show(&repo, &request, &call.confined),
                    BackendRequest::GitAdd(_) => add(&repo, &call.confined),
                    BackendRequest::GitCommit(request) => commit(&repo, &request),
                    BackendRequest::GitRestore(request) => restore(&repo, &request, &call.confined),
                    _ => unreachable!("matched above"),
                };
                let _ = std::fs::remove_dir_all(&repo.home);
                result?
            }
            _ => {
                return Err(invalid(format!(
                    "{} is not a git operation",
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

impl GitBackend {
    /// The token's worktree, checked to be one, with what git runs with.
    fn open(&self, call: &Call<'_>) -> Result<Repo, Failure> {
        let claims = call.claims;
        let workspace = PathBuf::from(&claims.workspace);
        let dir = open_workspace(&self.roots, &workspace)?;
        match lstat_at(&dir, OsStr::new(".git")) {
            Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFREG => {}
            Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFLNK => {
                return Err(violation("the workspace's .git is a symlink"));
            }
            Ok(_) => {
                return Err(violation(
                    "the workspace is not a worktree (its .git is not a file)",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(backend("the workspace is not a git worktree"));
            }
            Err(error) => return Err(backend(format!("look at the workspace's .git: {error}"))),
        }
        let (mut file, _) = open_regular(&dir, OsStr::new(".git"), ".git")?;
        drop(dir);
        let mut pointer = Vec::new();
        Read::by_ref(&mut file)
            .take(4096)
            .read_to_end(&mut pointer)
            .map_err(|error| backend(format!("read the workspace's .git: {error}")))?;
        let (gitdir, common) = self.resolve_gitdir(&workspace, &pointer)?;
        let home = std::env::temp_dir().join(format!("dagq-broker-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).map_err(|error| backend(format!("make git's HOME: {error}")))?;
        let mut repo = Repo {
            program: self.program.clone(),
            workspace,
            gitdir,
            common,
            home,
            path: self.path.clone(),
            committer: (
                claims.committer.name.clone(),
                claims.committer.email.clone(),
            ),
            branch: claims.branch.clone(),
            config: CONFIG.iter().map(|entry| (*entry).to_owned()).collect(),
            timeout: Duration::from_secs(call.limits.exec_timeout_secs),
            limit: call.limits.output_limit_bytes,
        };
        let checked = repo.filter_overrides();
        match checked {
            Ok(overrides) => repo.config.extend(overrides),
            Err(failure) => {
                let _ = std::fs::remove_dir_all(&repo.home);
                return Err(failure);
            }
        }
        Ok(repo)
    }

    /// The gitdir and the common dir of the workspace's worktree, from the
    /// `gitdir: <path>` its `.git` holds, read once here and then given to
    /// every git process (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`), so
    /// a `.git` rewritten between processes is never read. The gitdir must
    /// be `<common>/worktrees/<name>` outside every mounted root (a gitdir
    /// the worker made in a workspace would give it the repository's
    /// config), its `commondir` must lead to that common dir, and its
    /// `gitdir` must point back to the workspace's `.git` (not the gitdir
    /// of the main checkout or of another run's worktree).
    fn resolve_gitdir(
        &self,
        workspace: &Path,
        pointer: &[u8],
    ) -> Result<(PathBuf, PathBuf), Failure> {
        let not_a_worktree = || violation("the workspace's .git does not name a worktree's gitdir");
        let named = trim(pointer)
            .strip_prefix(b"gitdir: ")
            .ok_or_else(not_a_worktree)?;
        let named = PathBuf::from(OsString::from_vec(named.to_vec()));
        if !named.is_absolute() {
            return Err(not_a_worktree());
        }
        let gitdir = named.canonicalize().map_err(|_| not_a_worktree())?;
        let workspace = canonical(workspace)?;
        let inside_a_root = |path: &Path| {
            self.roots.iter().any(|root| {
                path.starts_with(root)
                    || root.canonicalize().is_ok_and(|root| path.starts_with(root))
            })
        };
        if gitdir.starts_with(&workspace) || inside_a_root(&gitdir) {
            return Err(violation(
                "the workspace's .git names a gitdir inside the runs",
            ));
        }
        let worktrees = gitdir.parent().ok_or_else(not_a_worktree)?;
        let common = worktrees.parent().ok_or_else(not_a_worktree)?.to_path_buf();
        if worktrees.file_name() != Some(OsStr::new("worktrees")) {
            return Err(not_a_worktree());
        }
        let commondir = std::fs::read(gitdir.join("commondir")).map_err(|_| not_a_worktree())?;
        let commondir = gitdir.join(PathBuf::from(OsString::from_vec(trim(&commondir).to_vec())));
        if commondir.canonicalize().ok().as_ref() != Some(&common) {
            return Err(not_a_worktree());
        }
        let back = std::fs::read(gitdir.join("gitdir")).map_err(|_| not_a_worktree())?;
        let back = PathBuf::from(OsString::from_vec(trim(&back).to_vec()));
        let points_back = back.parent().and_then(|parent| parent.canonicalize().ok());
        if back.file_name() != Some(OsStr::new(".git")) || points_back != Some(workspace) {
            return Err(violation(
                "the workspace's .git names the gitdir of another worktree",
            ));
        }
        Ok((gitdir, common))
    }
}

/// One operation's git: the worktree, the environment and the limits.
#[derive(Debug)]
struct Repo {
    program: PathBuf,
    workspace: PathBuf,
    /// The worktree's gitdir and the repository's common dir, resolved once.
    gitdir: PathBuf,
    common: PathBuf,
    /// A fresh, empty directory for `HOME`, removed after the operation.
    home: PathBuf,
    path: OsString,
    committer: (String, String),
    /// The token's branch, `dagq/<run id>`: the only one `git.commit` moves.
    branch: String,
    config: Vec<String>,
    timeout: Duration,
    limit: u64,
}

/// What a git process gave back.
#[derive(Debug)]
struct Ran {
    status: ExitStatus,
    stdout: Vec<u8>,
    /// Stdout was over the limit and cut at it.
    truncated: bool,
    stderr: Vec<u8>,
}

impl Ran {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim_end().to_owned()
    }
}

impl Repo {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        let (name, email) = &self.committer;
        command
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", &self.home)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "false")
            .env("GIT_TRACE2", "0")
            .env("GIT_TRACE2_EVENT", "0")
            .env("GIT_TRACE2_PERF", "0")
            .env("GIT_DIR", &self.gitdir)
            .env("GIT_COMMON_DIR", &self.common)
            .env("GIT_WORK_TREE", &self.workspace)
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .arg("-C")
            .arg(&self.workspace)
            .arg("--no-pager");
        let mut safe = OsString::from("safe.directory=");
        safe.push(&self.workspace);
        command.arg("-c").arg(safe);
        for entry in &self.config {
            command.arg("-c").arg(entry);
        }
        command.process_group(0);
        command
    }

    /// Run `git <args>` with `stdin`, within the time and output limits.
    fn run<S: AsRef<OsStr>>(&self, args: &[S], stdin: Option<&[u8]>) -> Result<Ran, Failure> {
        let mut command = self.command();
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| backend(format!("run git: {error}")))?;
        let writer = stdin.map(|bytes| {
            let mut pipe = child.stdin.take().expect("piped stdin");
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            })
        });
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let stdout = capture(child.stdout.take().expect("piped stdout"), limit);
        let stderr = capture(child.stderr.take().expect("piped stderr"), STDERR_LIMIT);
        let deadline = Instant::now() + self.timeout;
        let group = libc::pid_t::try_from(child.id()).ok();
        let status = wait(&mut child, deadline);
        // A process git left behind can hold the pipes open after git ended:
        // it gets until the same deadline, then its group is killed, and a
        // reader still blocked (a process that left the group) is left
        // behind rather than waited for.
        let done = || stdout.is_finished() && stderr.is_finished();
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !done() {
            kill_group(group);
            let grace = Instant::now() + Duration::from_secs(1);
            while !done() && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let timed_out = || {
            Failure::new(
                ErrorCode::Timeout,
                format!("git ran out of its {} seconds", self.timeout.as_secs()),
            )
        };
        if !done() {
            return Err(timed_out());
        }
        let (stdout, truncated) = stdout.join().unwrap_or_default();
        let (stderr, _) = stderr.join().unwrap_or_default();
        if let Some(writer) = writer.filter(JoinHandle::is_finished) {
            let _ = writer.join();
        }
        let status = status.ok_or_else(timed_out)?;
        Ok(Ran {
            status,
            stdout,
            truncated,
            stderr,
        })
    }

    /// `run`, then refuse a failure, and output over the limit.
    fn ok<S: AsRef<OsStr>>(
        &self,
        what: &str,
        args: &[S],
        stdin: Option<&[u8]>,
    ) -> Result<Ran, Failure> {
        let ran = self.run(args, stdin)?;
        if ran.truncated {
            return Err(Failure::new(
                ErrorCode::OutputLimit,
                format!("git {what} said more than {} bytes", self.limit),
            ));
        }
        if !ran.status.success() {
            return Err(failed(what, &ran));
        }
        Ok(ran)
    }

    /// `filter.<driver>.clean=` / `smudge=` / `process=` and `required=false`
    /// for every driver the repository's config defines, so neither add nor
    /// status runs one.
    fn filter_overrides(&self) -> Result<Vec<String>, Failure> {
        let ran = self.run(
            &[
                "config",
                "--null",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.",
            ],
            None,
        )?;
        // 1: no such key.
        if !ran.status.success() && ran.status.code() != Some(1) {
            return Err(failed("config", &ran));
        }
        if ran.truncated {
            return Err(backend("the repository's config defines too many filters"));
        }
        let mut drivers: Vec<String> = ran
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|key| {
                let key = std::str::from_utf8(key).ok()?;
                let driver = key.strip_prefix("filter.")?.rsplit_once('.')?.0;
                Some(driver.to_owned())
            })
            .collect();
        drivers.sort();
        drivers.dedup();
        Ok(drivers
            .iter()
            .flat_map(|driver| {
                ["clean=", "smudge=", "process=", "required=false"]
                    .map(|setting| format!("filter.{driver}.{setting}"))
            })
            .collect())
    }
}

fn status(repo: &Repo) -> Result<Vec<u8>, Failure> {
    let branch = repo.run(&["symbolic-ref", "-q", "--short", "HEAD"], None)?;
    let branch = if branch.status.success() {
        Some(branch.text())
    } else if branch.status.code() == Some(1) {
        None
    } else {
        return Err(failed("symbolic-ref", &branch));
    };
    let ran = repo.ok(
        "status",
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
            "--no-renames",
        ],
        None,
    )?;
    let mut entries = Vec::new();
    let mut fields = ran.stdout.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&field[..2]).into_owned();
        if status.contains(['R', 'C']) {
            // The path it came from follows.
            fields.next();
        }
        entries.push(git::StatusEntry {
            path: String::from_utf8_lossy(&field[3..]).into_owned(),
            status,
        });
    }
    answer(&git::StatusResponse { branch, entries })
}

fn diff(repo: &Repo, request: &git::DiffRequest, confined: &[PathBuf]) -> Result<Vec<u8>, Failure> {
    let mut args: Vec<OsString> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--ignore-submodules=all",
    ]
    .map(OsString::from)
    .to_vec();
    if request.staged {
        args.push("--cached".into());
    }
    args.push("--".into());
    args.extend(pathspecs(&repo.workspace, confined)?);
    let ran = repo.run(&args, None)?;
    if !ran.truncated && !ran.status.success() {
        return Err(failed("diff", &ran));
    }
    answer(&git::DiffResponse {
        truncated: ran.truncated,
        diff: cut_text(ran.stdout, ran.truncated),
    })
}

/// Output as text; when it was cut at the limit, cut back to a whole
/// character first.
fn cut_text(mut bytes: Vec<u8>, truncated: bool) -> String {
    if truncated {
        while std::str::from_utf8(&bytes).is_err_and(|error| error.error_len().is_none()) {
            bytes.pop();
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn show(repo: &Repo, request: &git::ShowRequest, confined: &[PathBuf]) -> Result<Vec<u8>, Failure> {
    let revision = request.commit.as_deref().unwrap_or("HEAD");
    if revision.is_empty() || revision.starts_with('-') || revision.contains('\0') {
        return Err(invalid("the commit is not a revision"));
    }
    let resolved = repo.run(
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{revision}^{{commit}}"),
        ],
        None,
    )?;
    if !resolved.status.success() {
        return Err(backend(format!("{revision} names no commit")));
    }
    let commit = resolved.text();
    // Only the run branch's own history: not another run's branch.
    let reached = repo.run(&["merge-base", "--is-ancestor", &commit, "HEAD"], None)?;
    match reached.status.code() {
        Some(0) => {}
        Some(1) => {
            return Err(violation(format!(
                "{revision} is not in the history of HEAD"
            )));
        }
        _ => return Err(failed("merge-base", &reached)),
    }
    let mut args: Vec<OsString> = [
        "show",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-show-signature",
        "--ignore-submodules=all",
        "--format=medium",
        &commit,
        "--",
    ]
    .map(OsString::from)
    .to_vec();
    args.extend(pathspecs(&repo.workspace, confined)?);
    let ran = repo.run(&args, None)?;
    if !ran.truncated && !ran.status.success() {
        return Err(failed("show", &ran));
    }
    answer(&git::ShowResponse {
        commit,
        truncated: ran.truncated,
        show: cut_text(ran.stdout, ran.truncated),
    })
}

fn restore(
    repo: &Repo,
    request: &git::RestoreRequest,
    confined: &[PathBuf],
) -> Result<Vec<u8>, Failure> {
    if confined.is_empty() {
        return Err(invalid("git.restore names at least one path"));
    }
    let mut args: Vec<OsString> = vec!["restore".into()];
    if request.staged {
        // The index from HEAD; the worktree is left as it is.
        args.extend(["--staged".into(), "--source=HEAD".into()]);
    } else {
        // The worktree from the index.
        args.push("--worktree".into());
    }
    args.push("--".into());
    args.extend(pathspecs(&repo.workspace, confined)?);
    repo.ok("restore", &args, None)?;
    answer(&git::RestoreResponse {})
}

fn log(repo: &Repo, request: &git::LogRequest) -> Result<Vec<u8>, Failure> {
    let limit = request.limit.unwrap_or(LOG_DEFAULT).min(LOG_MAX);
    let ran = repo.ok(
        "log",
        &[
            "log".to_owned(),
            format!("--max-count={limit}"),
            "-z".to_owned(),
            "--no-color".to_owned(),
            "--no-show-signature".to_owned(),
            "--format=%H%x00%an%x00%at%x00%s".to_owned(),
        ],
        None,
    )?;
    let fields: Vec<_> = ran.stdout.split(|byte| *byte == 0).collect();
    let commits = fields
        .chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| {
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            git::Commit {
                commit: text(chunk[0]),
                author: text(chunk[1]),
                time: text(chunk[2]).parse().unwrap_or_default(),
                subject: text(chunk[3]),
            }
        })
        .collect();
    answer(&git::LogResponse { commits })
}

fn add(repo: &Repo, confined: &[PathBuf]) -> Result<Vec<u8>, Failure> {
    if confined.is_empty() {
        return Err(invalid("git.add names at least one path"));
    }
    let mut args: Vec<OsString> = vec!["add".into(), "--".into()];
    args.extend(pathspecs(&repo.workspace, confined)?);
    repo.ok("add", &args, None)?;
    answer(&git::AddResponse {})
}

fn commit(repo: &Repo, request: &git::CommitRequest) -> Result<Vec<u8>, Failure> {
    let message = request.message.trim();
    if message.is_empty() {
        return Err(invalid("the commit message is empty"));
    }
    let branch = format!("refs/heads/{}", repo.branch);
    let head = repo.run(&["symbolic-ref", "-q", "HEAD"], None)?;
    match head.status.code() {
        Some(0) if head.text() == branch => {}
        Some(0) => {
            return Err(violation(format!(
                "HEAD is {}, not the run's branch {branch}",
                head.text()
            )));
        }
        Some(1) => {
            return Err(violation(format!(
                "HEAD is detached, not on the run's branch {branch}"
            )));
        }
        _ => return Err(failed("symbolic-ref", &head)),
    }
    let parent = repo
        .ok(
            "rev-parse",
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{branch}^{{commit}}"),
            ],
            None,
        )
        .map_err(|_| backend(format!("{branch} has no commit to build on")))?
        .text();
    let tree = repo.ok("write-tree", &["write-tree"], None)?.text();
    let parent_tree = repo
        .ok(
            "rev-parse",
            &["rev-parse", &format!("{parent}^{{tree}}")],
            None,
        )?
        .text();
    if tree == parent_tree {
        return Err(backend("nothing to commit: the index is the branch's tree"));
    }
    let message = format!("{message}\n");
    let commit = repo
        .ok(
            "commit-tree",
            &["commit-tree", "--no-gpg-sign", &tree, "-p", &parent],
            Some(message.as_bytes()),
        )?
        .text();
    // With the old value: the branch moves only from the parent read above.
    repo.ok(
        "update-ref",
        &[
            "update-ref",
            "-m",
            "commit (dagq-broker)",
            &branch,
            &commit,
            &parent,
        ],
        None,
    )?;
    answer(&git::CommitResponse { commit })
}

/// `:(top,literal)<path>` for each confined path (the workspace itself is
/// `:(top)`); the workspace's `.git` is refused as in the fs backend.
fn pathspecs(workspace: &Path, confined: &[PathBuf]) -> Result<Vec<OsString>, Failure> {
    confined
        .iter()
        .map(|path| {
            let relative = path
                .strip_prefix(workspace)
                .map_err(|_| violation("the path is outside the workspace"))?;
            let mut parts = Vec::new();
            for component in relative.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(part) => parts.push(part),
                    _ => return Err(violation("the path leaves the workspace")),
                }
            }
            if parts
                .first()
                .is_some_and(|first| first.as_bytes().eq_ignore_ascii_case(b".git"))
            {
                return Err(violation(
                    "the worktree's .git is not a path for the git operations",
                ));
            }
            if parts.is_empty() {
                return Ok(OsString::from(":(top)"));
            }
            let mut spec = OsString::from(":(top,literal)");
            spec.push(parts.iter().collect::<PathBuf>());
            Ok(spec)
        })
        .collect()
}

/// Read `pipe` to its end, keeping at most `limit` bytes; stop reading past
/// it (the writer then gets a broken pipe). The flag says it went over.
fn capture(mut pipe: impl Read + Send + 'static, limit: usize) -> JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return (kept, false),
                Ok(read) => {
                    if kept.len() + read > limit {
                        let room = limit - kept.len();
                        kept.extend_from_slice(&buffer[..room]);
                        return (kept, true);
                    }
                    kept.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return (kept, false),
            }
        }
    })
}

/// Wait for `child` until `deadline`; past it, kill its process group and
/// answer `None`.
fn wait(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                kill_group(libc::pid_t::try_from(child.id()).ok());
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// SIGKILL to the process group git led (`process_group(0)`).
fn kill_group(group: Option<libc::pid_t>) {
    if let Some(group) = group.filter(|group| *group > 0) {
        // SAFETY: a plain signal to the group of a child this backend made.
        unsafe { libc::kill(-group, libc::SIGKILL) };
    }
}

/// A failed git: its first `fatal:` / `error:` line (git's own words about
/// the command, not content), else the exit status.
fn failed(what: &str, ran: &Ran) -> Failure {
    let stderr = String::from_utf8_lossy(&ran.stderr);
    let reason = stderr
        .lines()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .map(|line| line.chars().take(200).collect::<String>());
    let exit = ran.status.code();
    let mut failure = backend(match reason {
        Some(reason) => format!("git {what} failed: {reason}"),
        None => format!("git {what} failed ({})", ran.status),
    });
    failure.exit_code = exit;
    failure
}

fn canonical(path: &Path) -> Result<PathBuf, Failure> {
    path.canonicalize()
        .map_err(|error| backend(format!("resolve the worktree: {error}")))
}

fn trim(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |at| at + 1);
    &bytes[..end]
}

fn answer<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, Failure> {
    encode(value).map_err(|error| backend(format!("encode the answer: {error}")))
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

#[cfg(test)]
mod tests;
