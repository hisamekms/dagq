//! [`Binaries`] on this machine: `cargo build --release --locked -p dagq`
//! in a checkout, a binary run as a child process for its version, a throwaway queue and the
//! queue's service started as a replacement's,
//! and the replacement of a file by a rename in its directory (ADR-0045
//! decisions 11, 12).

use crate::infrastructure::adapters::unpiped_output;
use crate::infrastructure::git_binary::git_executable;
use crate::infrastructure::queue_binary;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::application::install::{
    Binaries, E2eOutcome, E2eSettings, PendingMigration, ReleaseInstaller, SchemaCheck,
    parse_version, previous_path,
};

pub struct LocalBinaries;

/// How long the service an `install` starts may take to tell how its start
/// goes; once it does, a service applying migrations is waited for as long
/// as it lives.
const REPLACEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What a build of dagq's checkout builds: dagq alone.
pub const BUILD_ARGS: [&str; 5] = ["build", "--release", "--locked", "-p", "dagq"];

/// Run `binary` with `arguments`; its stdout when it exits 0, or an error
/// with what it wrote to stderr.
fn output(binary: &Path, arguments: &[&str]) -> Result<String> {
    output_with(binary, arguments, &[])
}

/// [`output`] with `envs` added to its environment.
fn output_with(binary: &Path, arguments: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let mut command = Command::new(binary);
    command.envs(envs.iter().copied());
    checked_output(&mut command, binary, arguments)
}

/// [`output`] as the user: no actor's variables reach it, so a read the
/// queue service answers goes as the user's whoever runs it.
fn user_output(binary: &Path, arguments: &[&str]) -> Result<String> {
    let mut command = Command::new(binary);
    for name in crate::domain::actor::ACTOR_ENV {
        command.env_remove(name);
    }
    checked_output(&mut command, binary, arguments)
}

/// Run `command` of `binary` with `arguments`: its stdout when it exits 0,
/// or an error with what it wrote to stderr.
fn checked_output(command: &mut Command, binary: &Path, arguments: &[&str]) -> Result<String> {
    // A client-mode `dagq`'s variables do not reach it: it opens the queue
    // it names (goal 82's stage (3)).
    for name in crate::domain::queue_service::CLIENT_ENV {
        command.env_remove(name);
    }
    run_checked(command, binary, arguments)
}

/// Run `command`, `binary` with `arguments`, as it is set: its stdout, or
/// an error with its stderr when it fails.
fn run_checked(command: &mut Command, binary: &Path, arguments: &[&str]) -> Result<String> {
    // Not `output()`: its pipes can be inherited by what the supervisor
    // spawns meanwhile and then not end (task 1273).
    let output = unpiped_output(command.args(arguments))
        .with_context(|| format!("run {}", binary.display()))?;
    ensure!(
        output.status.success(),
        "{} {} exited with {}: {}",
        binary.display(),
        arguments.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn json_output(binary: &Path, arguments: &[&str]) -> Result<Value> {
    let text = output(binary, arguments)?;
    serde_json::from_str(&text).with_context(|| format!("read what {} printed", binary.display()))
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// The command of the `up` of `binary` that starts a supervisor again, as
/// this process's caller: the token in `DAGQ_SERVICE_CREDENTIAL_FILE` that
/// `env` holds (an inbox's `install`), or the user's with none, so `up`
/// reaches the queue only through its service. No socket is named to it: it
/// finds the service of the queue it names.
fn up_command(binary: &Path, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Command {
    use crate::domain::queue_service::CREDENTIAL_FILE_ENV;
    let mut command = Command::new(binary);
    for name in crate::domain::queue_service::CLIENT_ENV {
        command.env_remove(name);
    }
    command.env(crate::application::lifecycle::UP_RESTART_ENV, "1");
    if let Some(file) = env(CREDENTIAL_FILE_ENV) {
        command.env(CREDENTIAL_FILE_ENV, file);
    }
    command
}

impl Binaries for LocalBinaries {
    fn build(&self, checkout: &Path) -> Result<PathBuf> {
        let status = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args(BUILD_ARGS)
            .current_dir(checkout)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .context("run cargo build")?;
        ensure!(
            status.success(),
            "cargo build --release exited with {status}"
        );
        let target = match env::var_os("CARGO_TARGET_DIR") {
            Some(dir) => checkout.join(dir),
            None => checkout.join("target"),
        };
        Ok(target.join("release").join("dagq"))
    }

    fn version(&self, binary: &Path) -> Result<String> {
        parse_version(&output(binary, &["--version"])?)
    }

    fn probe(&self, binary: &Path) -> Result<()> {
        let dir = env::temp_dir().join(format!(
            "dagq-install-probe-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let db = dir.join("queue.db");
        let result = (|| {
            let db = text(&db)?;
            json_output(binary, &["--db", db, "init"])?;
            // A person's read goes through the queue's service, which the
            // build never opens the DB for (ADR-t3105-1 decision 3): its
            // service is started for the read and stopped after. A build
            // before the queue service has no `service` and reads the DB.
            // The read is the user's: an install the inbox runs has no
            // token for this queue's service.
            let served = json_output(binary, &["--db", db, "service", "start"]).is_ok();
            let read = user_output(binary, &["--db", db, "list"]).and_then(|text| {
                serde_json::from_str::<Value>(&text)
                    .with_context(|| format!("read what {} printed", binary.display()))
            });
            if served {
                let _ = output(binary, &["--db", db, "service", "stop"]);
            }
            read.map(drop)
        })();
        let _ = fs::remove_dir_all(&dir);
        result
    }

    fn takes_handoff(&self, binary: &Path) -> bool {
        // Clap refuses an unknown argument before it prints the help.
        output(binary, &["supervise", "--handoff-token", "probe", "--help"]).is_ok()
    }

    fn schema(&self, binary: &Path, db: &Path) -> Result<SchemaCheck> {
        let report = json_output(binary, &["--db", text(db)?, "migrate", "--check"])?;
        let pending = report["pending"]
            .as_array()
            .context("migrate --check reported no pending list")?
            .iter()
            .map(|migration| {
                Ok(PendingMigration {
                    version: migration["version"]
                        .as_i64()
                        .context("a pending migration without a version")?,
                    compatible: migration["compatible"] == true,
                })
            })
            .collect::<Result<_>>()?;
        Ok(SchemaCheck {
            pending,
            opens: report["opens"] == true,
        })
    }

    fn serve(&self, binary: &Path, db: &Path, breaking: bool) -> Result<Value> {
        use crate::application::queue_service::QueueServiceControl;
        let _grant = queue_binary::Grant::to(db, binary)?;
        let replaced = crate::infrastructure::queue_service::SystemQueueService::new(db, binary)
            .replace(breaking, false, REPLACEMENT_TIMEOUT)?;
        Ok(replaced.migrated.unwrap_or(Value::Null))
    }

    fn hold_service(&self, db: &Path) -> Result<crate::application::queue_service::ServiceHold> {
        use crate::application::queue_service::QueueServiceControl;
        crate::infrastructure::queue_service::SystemQueueService::new(db, Path::new("dagq")).hold()
    }

    fn service_runs(&self, db: &Path) -> bool {
        let queue_dir = crate::infrastructure::queue_service::queue_dir_of(db);
        crate::infrastructure::queue_service::probe(&queue_dir).state
            == crate::domain::queue_service::ServiceState::Running
    }

    fn adopt(&self, db: &Path, target: &Path) -> Result<()> {
        queue_binary::write(db, &queue_binary::QueueBinary::new(target))
    }

    fn replace(&self, source: &Path, target: &Path) -> Result<()> {
        let dir = target
            .parent()
            .with_context(|| format!("{} has no directory", target.display()))?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let name = target
            .file_name()
            .with_context(|| format!("{} has no file name", target.display()))?
            .to_string_lossy()
            .into_owned();
        let pid = std::process::id();
        let staged = dir.join(format!(".{name}.install-{pid}"));
        fs::copy(source, &staged)
            .with_context(|| format!("copy {} to {}", source.display(), staged.display()))?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        if !target.exists() {
            fs::rename(&staged, target)
                .with_context(|| format!("move the new binary to {}", target.display()))?;
            return Ok(());
        }
        // The binary in place keeps its inode (a running process keeps
        // using it) under a second name, which then becomes `.previous`.
        let kept = dir.join(format!(".{name}.previous-{pid}"));
        let _ = fs::remove_file(&kept);
        if fs::hard_link(target, &kept).is_err() {
            fs::copy(target, &kept)
                .with_context(|| format!("keep {} as {}", target.display(), kept.display()))?;
        }
        fs::rename(&staged, target)
            .with_context(|| format!("move the new binary to {}", target.display()))?;
        fs::rename(&kept, previous_path(target))
            .with_context(|| format!("keep the replaced binary next to {}", target.display()))?;
        Ok(())
    }

    fn restore(&self, target: &Path) -> Result<()> {
        let previous = previous_path(target);
        if !previous.is_file() {
            bail!("no previous binary at {}", previous.display());
        }
        fs::rename(&previous, target)
            .with_context(|| format!("move {} back to {}", previous.display(), target.display()))
    }

    /// Every binary run here is the `up` that starts a supervisor again, so
    /// it runs as the runtime's restart (`UP_RESTART_ENV`).
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let mut command = up_command(binary, |name| env::var_os(name));
        let text = run_checked(&mut command, binary, &arguments)?;
        serde_json::from_str(&text)
            .with_context(|| format!("read what {} printed", binary.display()))
    }

    fn checkout(&self, repository: &Path, checkout: &Path, commit: &str) -> Result<()> {
        let git = |dir: &Path, arguments: &[&str]| -> Result<()> {
            let output = unpiped_output(
                Command::new(git_executable()?)
                    .arg("-C")
                    .arg(dir)
                    .args(arguments),
            )
            .context("run git")?;
            ensure!(
                output.status.success(),
                "git {} in {} exited with {}: {}",
                arguments.join(" "),
                dir.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        };
        if checkout.join(".git").exists() {
            git(
                checkout,
                &["checkout", "--quiet", "--detach", "--force", commit],
            )?;
            return git(checkout, &["clean", "--quiet", "-fdx"]);
        }
        if let Some(parent) = checkout.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        // A checkout whose directory was removed is still registered.
        git(repository, &["worktree", "prune"])?;
        git(
            repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                "--force",
                text(checkout)?,
                commit,
            ],
        )
    }

    fn build_into(
        &self,
        checkout: &Path,
        target_dir: &Path,
        target_env: &str,
        command: Option<&str>,
        log: &Path,
    ) -> Result<PathBuf> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("open {}", log.display()))?;
        let mut build = match command {
            Some(command) => {
                let mut shell = Command::new("/bin/sh");
                shell.args(["-c", command]);
                shell
            }
            None => {
                let mut cargo =
                    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
                cargo.args(BUILD_ARGS);
                cargo
            }
        };
        let status = build
            .current_dir(checkout)
            .env(target_env, target_dir)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .status()
            .context("run the build")?;
        ensure!(
            status.success(),
            "the build exited with {status}; see {}",
            log.display()
        );
        let binary = target_dir.join("release").join("dagq");
        ensure!(
            binary.is_file(),
            "the build left no binary at {}",
            binary.display()
        );
        Ok(binary)
    }

    fn e2e(
        &self,
        checkout: &Path,
        target_dir: Option<&Path>,
        settings: &E2eSettings,
    ) -> Result<E2eOutcome> {
        super::e2e_gate::run(checkout, target_dir, settings)
    }
}

/// [`ReleaseInstaller`] on this machine: `program` (`cargo` unless a test
/// gives a stub) runs `install --locked dagq@<version> --root <root>
/// --target-dir <target_dir>` (ADR-t618-1 decision 5).
#[derive(Debug, Clone)]
pub struct CargoInstaller {
    pub program: PathBuf,
}

impl Default for CargoInstaller {
    fn default() -> Self {
        Self {
            program: PathBuf::from("cargo"),
        }
    }
}

impl ReleaseInstaller for CargoInstaller {
    fn install(
        &self,
        version: &str,
        root: &Path,
        target_dir: &Path,
        log: &Path,
    ) -> Result<PathBuf> {
        if let Some(dir) = log.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .with_context(|| format!("open {}", log.display()))?;
        let status = Command::new(&self.program)
            .args(["install", "--locked"])
            .arg(format!(
                "{}@{version}",
                crate::domain::source_repository::PACKAGE
            ))
            .arg("--root")
            .arg(root)
            .arg("--target-dir")
            .arg(target_dir)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .status();
        let status = match status {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "{} was not found: installing a release needs cargo (and a Rust toolchain that \
builds dagq {version})",
                self.program.display()
            ),
            Err(error) => {
                return Err(error).with_context(|| format!("run {}", self.program.display()));
            }
        };
        ensure!(
            status.success(),
            "`{} install --locked dagq@{version}` exited with {status}; see {}",
            self.program.display(),
            log.display()
        );
        let binary = root.join("bin").join("dagq");
        ensure!(
            binary.is_file(),
            "cargo install left no binary at {}",
            binary.display()
        );
        Ok(binary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{actor::ROLE_ENV, queue_service::CREDENTIAL_FILE_ENV};
    use std::ffi::{OsStr, OsString};

    /// What `command` sets (`Some`) or removes (`None`) of `name`; absent
    /// when it leaves `name` as this process has it.
    fn set(command: &Command, name: &str) -> Option<Option<OsString>> {
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value.map(OsStr::to_owned))
    }

    #[test]
    fn an_install_s_up_calls_as_its_caller() {
        let given = |name: &str| match name {
            CREDENTIAL_FILE_ENV => Some(OsString::from("/q/inbox-token")),
            ROLE_ENV => Some(OsString::from("inbox")),
            _ => None,
        };
        let binary = Path::new("dagq");
        // An inbox's `install`: its own token and role go on.
        let inherited = up_command(binary, given);
        assert_eq!(
            set(&inherited, CREDENTIAL_FILE_ENV),
            Some(Some("/q/inbox-token".into()))
        );
        assert_eq!(set(&inherited, ROLE_ENV), None);
        assert_eq!(
            set(&inherited, crate::application::lifecycle::UP_RESTART_ENV),
            Some(Some("1".into()))
        );
        // The user's: no token.
        let none = up_command(binary, |_| None);
        assert_eq!(set(&none, CREDENTIAL_FILE_ENV), Some(None));
    }
}
