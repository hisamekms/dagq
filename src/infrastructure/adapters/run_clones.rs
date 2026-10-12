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
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    sync::{Arc, Mutex},
};

use super::*;
use crate::application::CloneFacts;
use crate::domain::run_clone::{
    CLONE_MAIN_REF, MAIN_BUNDLE_FILE, RUN_BUNDLE_FILE, RUN_FILE_LIMIT, RUN_STATE_FILE, RunState,
    bundle_head, guard_env, guard_holds, guard_overrides, is_clone_path, landing_of, run_dir_of,
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
        guard_overrides(&names).map_err(|why| anyhow::anyhow!("{why}"))
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
            anyhow::anyhow!(
                "the hooks and drivers of the git configuration in {} cannot be turned off: {why}",
                dir.display()
            )
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
    /// sends a pack rather than linking object files.
    pub fn create_clone(&self, run: &TaskRun, carried: Option<&str>) -> Result<String> {
        let clone = Path::new(run.worktree_path().context("missing worktree")?);
        ensure!(run.in_container(), "run {} has no clone", run.id());
        let branch = run.branch().context("missing branch")?;
        let base = run.base_commit().as_str();
        let guarded = self.guarded_repository()?;
        if let Some(parent) = clone.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        ensure!(
            entry(clone)?.is_none(),
            "{} is there already",
            clone.display()
        );
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
        match run.landing_path() {
            Some(path) => self.remove_landing_at(&path),
            None => Ok(()),
        }
    }

    /// Remove the landing worktree at `path` (its registration, then what
    /// is left there, without following a symlink).
    fn remove_landing_at(&self, path: &Path) -> Result<()> {
        let path = path.to_path_buf();
        // Nothing is removed through a landing directory that is not one.
        if let Some(meta) = entry(path.parent().context("no landing directory")?)?
            && (meta.file_type().is_symlink() || !meta.is_dir())
        {
            bail!(
                "{} is a symlink or no directory; nothing is removed through it",
                path.parent().unwrap_or(&path).display()
            );
        }
        let guarded = self.guarded_repository()?;
        let registered = guarded
            .worktrees()?
            .iter()
            .any(|(listed, _)| same_path(listed, &path));
        if registered {
            let primary = guarded.primary_worktree()?;
            let _ = output(&mut guarded.remove_worktree_command(&primary, &path));
        }
        remove_entry(&path)?;
        guarded.prune_worktrees()
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
        self.remove_landing_at(&landing)?;
        remove_entry(clone)?;
        self.guarded_repository()?.delete_branch(branch)?;
        for name in [RUN_BUNDLE_FILE, RUN_STATE_FILE, MAIN_BUNDLE_FILE] {
            remove_entry(&run_dir.join(name))?;
        }
        Ok(())
    }
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
