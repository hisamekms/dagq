//! The host's backend of a run review's program jobs (ADR-t1895-2
//! decision 5): a program runs on this machine with the run's worktree as
//! its working directory and the narrowed environment of
//! [`PROGRAM_ENV`]. A script runs from a copy of its text at the landing
//! branch's commit written under the attempt's scratch directory, never
//! from the worktree, so a worker's change to it does not run.

use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use super::passed_env::PassedEnv;
use crate::application::{CommandSpec, ReviewProgramBackend, review_programs::SnapshotProgram};
use crate::domain::review_programs::ProgramRun;

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
/// narrowed by [`PROGRAM_ENV`].
pub struct HostPrograms {
    inherited: Vec<(OsString, OsString)>,
}

impl HostPrograms {
    /// With this process's environment.
    pub fn new() -> Self {
        Self::inheriting(std::env::vars_os())
    }

    /// With `inherited` as the starting process's environment.
    pub fn inheriting(inherited: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self {
            inherited: inherited.into_iter().collect(),
        }
    }
}

impl Default for HostPrograms {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewProgramBackend for HostPrograms {
    fn command(
        &self,
        program: &SnapshotProgram,
        worktree: &Path,
        scratch: &Path,
    ) -> Result<CommandSpec> {
        let mut command = match &program.program.run {
            ProgramRun::Command(argv) => {
                let (name, args) = argv
                    .split_first()
                    .context("the program's command names no program")?;
                let mut command = CommandSpec::new(name);
                command.args(args);
                command
            }
            ProgramRun::Script { path, args } => {
                let text = program.script.as_deref().with_context(|| {
                    format!("the script {path} was not read from the landing branch")
                })?;
                let mut command = CommandSpec::new(write_script(scratch, path, text)?);
                command.args(args);
                command
            }
        };
        command
            .env_clear()
            .envs(PROGRAM_ENV.filter(self.inherited.iter().cloned()))
            .current_dir(worktree);
        Ok(command)
    }
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
    use std::ffi::OsStr;

    fn snapshot(run: ProgramRun, script: Option<&str>) -> SnapshotProgram {
        SnapshotProgram {
            program: ReviewProgram {
                name: "check".to_owned(),
                run,
                paths: vec!["**".to_owned()],
                timeout_secs: None,
            },
            matched: vec!["a".to_owned()],
            script: script.map(str::to_owned),
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

    /// A program job is given no credential (ADR-t1895-2 decision 6): not
    /// cmux's socket password or any `CMUX_*`, which the e2e gate is given,
    /// nor what reaches the queue service or a broker, though the
    /// supervisor has them; only the narrowed names, from an emptied
    /// environment, in the run's worktree.
    #[test]
    fn a_program_job_is_given_no_credential_and_no_cmux_or_queue() {
        let scratch = tempfile::tempdir().unwrap();
        let command = HostPrograms::inheriting(inherited())
            .command(
                &snapshot(
                    ProgramRun::Command(vec!["cargo".to_owned(), "fmt".to_owned()]),
                    None,
                ),
                Path::new("/runs/r/worktree"),
                scratch.path(),
            )
            .unwrap();
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
        assert_eq!(command.get_program(), "cargo");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["fmt"]);
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/runs/r/worktree"))
        );
    }

    /// A script runs from its committed text written under the scratch
    /// directory, executable, with its arguments; what an earlier attempt
    /// left there is replaced, and a link put in the scratch's place is
    /// removed, never followed. One whose text was not read is an error.
    #[test]
    fn a_script_runs_from_its_committed_text_outside_the_worktree() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("review-program-1-check");
        let run = ProgramRun::Script {
            path: "scripts/check.sh".to_owned(),
            args: vec!["--quiet".to_owned()],
        };
        let write = |text: &str| {
            HostPrograms::inheriting(inherited())
                .command(
                    &snapshot(run.clone(), Some(text)),
                    Path::new("/runs/r/worktree"),
                    &scratch,
                )
                .unwrap()
        };
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
        assert!(
            HostPrograms::inheriting(inherited())
                .command(&snapshot(run, None), Path::new("/w"), &scratch)
                .is_err()
        );
    }
}
