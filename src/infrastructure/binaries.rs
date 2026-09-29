//! [`Binaries`] on this machine: `cargo build --release --locked -p dagq -p
//! dagq-broker-client` in a checkout (dagq and the worker's broker client,
//! which go in place together (ADR-t827-1 decision 5); not the server,
//! which is built into the broker's image), a binary run
//! as a child process for its version, a throwaway queue and its migrations,
//! and the replacement of a file by a rename in its directory (ADR-0045
//! decisions 11, 12).

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

/// What a build of dagq's checkout builds: dagq and the worker's client,
/// which lands next to it in the target (ADR-t827-1 decision 5).
pub const BUILD_ARGS: [&str; 7] = [
    "build",
    "--release",
    "--locked",
    "-p",
    "dagq",
    "-p",
    "dagq-broker-client",
];

/// Run `binary` with `arguments`; its stdout when it exits 0, or an error
/// with what it wrote to stderr.
fn output(binary: &Path, arguments: &[&str]) -> Result<String> {
    output_with(binary, arguments, &[])
}

/// [`output`] with `envs` added to its environment.
fn output_with(binary: &Path, arguments: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let output = Command::new(binary)
        .args(arguments)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
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
            json_output(binary, &["--db", text(&db)?, "init"])?;
            json_output(binary, &["--db", text(&db)?, "list"])?;
            Ok(())
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

    fn migrate(&self, binary: &Path, db: &Path) -> Result<Value> {
        json_output(binary, &["--db", text(db)?, "migrate"])
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

    fn set_aside(&self, target: &Path) -> Result<()> {
        fs::rename(target, previous_path(target))
            .with_context(|| format!("move {} aside", target.display()))
    }

    /// Every binary run here is the `up` that starts a supervisor again, so
    /// it runs as the runtime's restart (`UP_RESTART_ENV`).
    fn run(&self, binary: &Path, arguments: &[String]) -> Result<Value> {
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let text = output_with(
            binary,
            &arguments,
            &[(crate::application::lifecycle::UP_RESTART_ENV, "1")],
        )?;
        serde_json::from_str(&text)
            .with_context(|| format!("read what {} printed", binary.display()))
    }

    fn checkout(&self, repository: &Path, checkout: &Path, commit: &str) -> Result<()> {
        let git = |dir: &Path, arguments: &[&str]| -> Result<()> {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(arguments)
                .stdin(Stdio::null())
                .output()
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
            .env("CARGO_TARGET_DIR", target_dir)
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
        self.install_client(version, root, target_dir, log);
        Ok(binary)
    }
}

impl CargoInstaller {
    /// Install the worker's client of the same release beside dagq
    /// (ADR-t827-1 decision 8). A release without it on crates.io leaves
    /// none, and no client of another release, beside dagq: `install` then
    /// sets the one in place aside, and the broker is not used until both
    /// are there (decision 7).
    fn install_client(&self, version: &str, root: &Path, target_dir: &Path, log: &Path) {
        let client = crate::application::broker::client_path(&root.join("bin").join("dagq"));
        let _ = fs::remove_file(&client);
        let Ok(file) = fs::OpenOptions::new().create(true).append(true).open(log) else {
            return;
        };
        let Ok(stdout) = file.try_clone() else {
            return;
        };
        let installed = Command::new(&self.program)
            .args(["install", "--locked"])
            .arg(format!(
                "{}@{version}",
                crate::application::broker::CLIENT_BINARY
            ))
            .arg("--root")
            .arg(root)
            .arg("--target-dir")
            .arg(target_dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(file)
            .status()
            .is_ok_and(|status| status.success());
        if !installed {
            let _ = fs::remove_file(&client);
        }
    }
}
