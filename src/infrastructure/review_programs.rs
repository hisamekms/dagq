//! The host's backend of a run review's program jobs (ADR-t1895-2
//! decision 5): a program runs on this machine with the run's worktree as
//! its working directory and the narrowed environment of
//! [`PROGRAM_ENV`]. A script runs from a copy of its text at the landing
//! branch's commit written under the attempt's scratch directory, never
//! from the worktree, and the `PATH` it is given names neither the run's
//! worktree and directory nor the main checkout ([`narrowed_path`]), so a
//! worker's change to the script or a program put there does not run.

use anyhow::{Context, Result};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

use super::passed_env::PassedEnv;
use crate::application::{CommandSpec, ReviewProgramBackend, review_programs::SnapshotProgram};

/// What a program job is given of the supervisor's environment: what a
/// shell and the format checkers of cargo and rustup need to find their
/// tools, home, locale and scratch, and no credential (ADR-t1895-2
/// decision 6). Unlike the e2e gate's, it names no exception and no
/// `CMUX_*`, and none of the variables that reach the queue service or a
/// broker (`DAGQ_*`): a program review is a read-only check that calls
/// neither cmux nor the queue.
pub const PROGRAM_ENV: PassedEnv<'static> = PassedEnv {
    names: &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "TERM",
        "TZ",
        "TMPDIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
    ],
    prefixes: &["LC_"],
    exceptions: &[],
};

/// Runs the program jobs on the host, given the environment of `inherited`
/// narrowed by [`PROGRAM_ENV`] and its `PATH` by [`narrowed_path`] with
/// `repository`, the main checkout's root, among what it leaves out.
pub struct HostPrograms {
    repository: PathBuf,
    inherited: Vec<(OsString, OsString)>,
}

impl HostPrograms {
    /// With this process's environment.
    pub fn new(repository: impl Into<PathBuf>) -> Self {
        Self::inheriting(repository, env::vars_os())
    }

    /// With `inherited` as the starting process's environment.
    pub fn inheriting(
        repository: impl Into<PathBuf>,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        Self {
            repository: repository.into(),
            inherited: inherited.into_iter().collect(),
        }
    }
}

impl ReviewProgramBackend for HostPrograms {
    fn command(
        &self,
        program: &SnapshotProgram,
        worktree: &Path,
        run_dir: &Path,
        scratch: &Path,
    ) -> Result<CommandSpec> {
        let path = &program.program.script;
        let mut command = CommandSpec::new(write_script(scratch, path, &program.script)?);
        command.args(&program.program.args);
        let mut given = PROGRAM_ENV.filter(self.inherited.iter().cloned());
        if let Some(value) = given.remove(OsStr::new("PATH"))
            && let Some(narrowed) = narrowed_path(&value, &[worktree, run_dir, &self.repository])
        {
            given.insert("PATH".into(), narrowed);
        }
        command.env_clear().envs(given).current_dir(worktree);
        Ok(command)
    }
}

/// `path`, a `PATH`'s value, without the entries that could name a program
/// the worker put there: an empty or relative one (found from the working
/// directory, the run's worktree) and one in or under one of `roots` (the
/// run's worktree and directory and the main checkout), as written or with
/// its links resolved. `None` when no entry is left: an empty `PATH` would
/// search the working directory, and none makes a shell use its default.
pub fn narrowed_path(path: &OsStr, roots: &[&Path]) -> Option<OsString> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| [Some(lexical(root)), root.canonicalize().ok()])
        .flatten()
        .collect();
    let kept: Vec<PathBuf> = env::split_paths(path)
        .filter(|entry| {
            entry.is_absolute()
                && ![Some(lexical(entry)), entry.canonicalize().ok()]
                    .iter()
                    .flatten()
                    .any(|form| roots.iter().any(|root| form.starts_with(root)))
        })
        .collect();
    (!kept.is_empty()).then(|| env::join_paths(kept).expect("the entries came from one PATH"))
}

/// An absolute `path` with its `.` and `..` resolved by their words, not
/// the file system, so `/runs/r/worktree/../worktree/bin` is under
/// `/runs/r/worktree`.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Write the committed `text` of the script `path` to a new `scratch`
/// under its file name, executable by its owner only: what an earlier
/// attempt left there is removed (a link as a link, never followed), and
/// the directory and the file are made anew, never through a link.
fn write_script(scratch: &Path, path: &str, text: &str) -> Result<PathBuf> {
    match fs::symlink_metadata(scratch) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(scratch),
        Ok(_) => fs::remove_file(scratch),
        Err(_) => Ok(()),
    }
    .with_context(|| format!("remove {}", scratch.display()))?;
    if let Some(parent) = scratch.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(scratch)
        .with_context(|| format!("create {}", scratch.display()))?;
    let name = Path::new(path)
        .file_name()
        .with_context(|| format!("the script {path} names no file"))?;
    let file = scratch.join(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&file)
        .and_then(|mut opened| opened.write_all(text.as_bytes()))
        .with_context(|| format!("write {}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::review_programs::ReviewProgram;

    fn snapshot(text: &str) -> SnapshotProgram {
        SnapshotProgram {
            program: ReviewProgram {
                name: "check".to_owned(),
                script: "scripts/check.sh".to_owned(),
                args: vec!["--quiet".to_owned()],
                paths: vec!["**".to_owned()],
                timeout_secs: None,
            },
            matched: vec!["a".to_owned()],
            script: text.to_owned(),
        }
    }

    /// The supervisor's environment as a host where cmux, the queue
    /// service and a broker are reachable has it.
    fn inherited() -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/u"),
            ("LC_ALL", "C"),
            ("TMPDIR", "/tmp/u"),
            ("CMUX_SOCKET_PASSWORD", "socket-password"),
            ("CMUX_SOCKET_PATH", "/tmp/cmux.sock"),
            ("CMUX_WORKSPACE_ID", "w-1"),
            ("DAGQ_SERVICE_SOCKET", "/q/service.sock"),
            ("DAGQ_SERVICE_CREDENTIAL_FILE", "/q/credential"),
            ("DAGQ_BROKER_URL", "http://127.0.0.1:1"),
            ("DAGQ_BROKER_TOKEN_FILE", "/q/broker-token"),
            ("DAGQ_QUEUE", "/q/queue.db"),
            ("GH_TOKEN", "gh"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("LC_SECRET", "s"),
            ("OTHER", "x"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .to_vec()
    }

    fn command(inherited: Vec<(OsString, OsString)>, scratch: &Path, text: &str) -> CommandSpec {
        HostPrograms::inheriting("/repo", inherited)
            .command(
                &snapshot(text),
                Path::new("/runs/r/worktree"),
                Path::new("/runs/r"),
                scratch,
            )
            .unwrap()
    }

    /// A program job is given no credential (ADR-t1895-2 decision 6): not
    /// cmux's socket password or any `CMUX_*`, which the e2e gate is given,
    /// nor what reaches the queue service or a broker, though the
    /// supervisor has them; only the narrowed names, from an emptied
    /// environment, in the run's worktree.
    #[test]
    fn a_program_job_is_given_no_credential_and_no_cmux_or_queue() {
        let scratch = tempfile::tempdir().unwrap();
        let command = command(inherited(), scratch.path(), "#!/bin/sh\n");
        assert!(command.get_env_clear());
        let given: Vec<&OsStr> = command.get_envs().map(|(name, _)| name).collect();
        assert_eq!(given, ["HOME", "LC_ALL", "PATH", "TMPDIR"]);
        for name in [
            "CMUX_SOCKET_PASSWORD",
            "CMUX_SOCKET_PATH",
            "DAGQ_SERVICE_SOCKET",
            "DAGQ_SERVICE_CREDENTIAL_FILE",
            "DAGQ_BROKER_URL",
            "DAGQ_BROKER_TOKEN_FILE",
            "DAGQ_QUEUE",
            "GH_TOKEN",
        ] {
            assert!(!PROGRAM_ENV.passes(name), "{name}");
        }
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/runs/r/worktree"))
        );
    }

    /// The `PATH` a program job is given keeps the absolute entries
    /// outside the worker's reach and leaves out an empty or relative one
    /// and one in or under the run's worktree, the run's directory or the
    /// main checkout, as written, through `..`, or through a link; with
    /// nothing left the job is given no `PATH`.
    #[test]
    fn a_program_jobs_path_names_nothing_the_worker_can_write() {
        let root = tempfile::tempdir().unwrap();
        let run_dir = root.path().join("runs/r");
        let worktree = run_dir.join("worktree");
        let repository = root.path().join("repo");
        let host = root.path().join("host/bin");
        for dir in [&worktree.join("bin"), &repository.join("bin"), &host] {
            fs::create_dir_all(dir).unwrap();
        }
        let link = root.path().join("link");
        std::os::unix::fs::symlink(worktree.join("bin"), &link).unwrap();
        let entries = [
            "/usr/bin".to_owned(),
            String::new(),
            ".".to_owned(),
            "bin".to_owned(),
            "./target/debug".to_owned(),
            worktree.display().to_string(),
            worktree.join("bin").display().to_string(),
            run_dir.join("tools").display().to_string(),
            repository.join("bin").display().to_string(),
            root.path().join("host/../repo/bin").display().to_string(),
            format!("{}/../worktree/bin", worktree.display()),
            link.display().to_string(),
            host.display().to_string(),
            "/bin".to_owned(),
        ];
        let path = OsString::from(entries.join(":"));
        let narrowed = narrowed_path(&path, &[&worktree, &run_dir, &repository]).unwrap();
        assert_eq!(
            narrowed,
            OsString::from(format!("/usr/bin:{}:/bin", host.display()))
        );
        assert_eq!(
            narrowed_path(OsStr::new(".::bin"), &[&worktree, &run_dir]),
            None
        );
        let mut without = inherited();
        without.retain(|(name, _)| name != "PATH");
        without.push(("PATH".into(), ".:/runs/r/worktree/bin:/repo/x".into()));
        let scratch = tempfile::tempdir().unwrap();
        let given = command(without, scratch.path(), "#!/bin/sh\n");
        assert!(given.get_envs().all(|(name, _)| name != "PATH"));
        let given = command(inherited(), scratch.path(), "#!/bin/sh\n");
        assert!(
            given
                .get_envs()
                .any(|(name, value)| name == "PATH" && value == Some(OsStr::new("/usr/bin:/bin")))
        );
    }

    /// A script runs from its committed text written under the scratch
    /// directory, executable, with its arguments; what an earlier attempt
    /// left there is replaced, and a link put in the scratch's place is
    /// removed, never followed.
    #[test]
    fn a_script_runs_from_its_committed_text_outside_the_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("review-program-1-check");
        let write = |text: &str| command(inherited(), &scratch, text);
        let file = scratch.join("check.sh");
        let command = write("#!/bin/sh\necho first\n");
        assert_eq!(command.get_program(), file.as_os_str());
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--quiet"]);
        write("#!/bin/sh\necho main\n");
        assert_eq!(fs::read_to_string(&file).unwrap(), "#!/bin/sh\necho main\n");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A link in the scratch's place is not followed.
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(elsewhere.join("check.sh"), "kept").unwrap();
        fs::remove_dir_all(&scratch).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &scratch).unwrap();
        write("#!/bin/sh\necho main\n");
        assert!(!fs::symlink_metadata(&scratch).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(elsewhere.join("check.sh")).unwrap(),
            "kept"
        );
    }
}
