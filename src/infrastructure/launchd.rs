//! The launchd LaunchAgent that keeps a queue's supervisor resident. `up`
//! writes one plist per queue under `~/Library/LaunchAgents` and loads it
//! into the user's `gui/<uid>` domain; `down` unloads it. launchd restarts
//! the supervisor whenever it exits (`KeepAlive`), which is why stopping it
//! goes through `bootout` rather than a signal.
//!
//! launchd is macOS's: on another host nothing can be loaded, so `down`
//! finds no agent and `up` fails saying launchd mode needs macOS, rather
//! than with `launchctl` not found (goal 83, docs/design/linux-ci.md).
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use super::adapters::capture;
#[cfg(test)]
use crate::application::SupervisorEnvironment;
use crate::application::{AgentState, LaunchAgent};

pub use crate::application::lifecycle::{EXIT_TIMEOUT_SECS, LaunchAgentSpec};

/// `launchctl` against the `gui/<uid>` domain of the user running `up`.
pub struct Launchctl {
    pub uid: u32,
}

/// How long `install` waits for a previously loaded agent's process to
/// go away after its bootout before forcing it, and again after that.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(60);
const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether this host has launchd (macOS).
const HAS_LAUNCHD: bool = cfg!(target_os = "macos");

impl Launchctl {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self, label: &str) -> String {
        format!("{}/{label}", self.domain())
    }

    /// The service as launchd sees it: loaded (still listed) and its pid.
    /// A service stays listed after `bootout` until its process has exited.
    fn state(&self, label: &str) -> Result<AgentState> {
        if !HAS_LAUNCHD {
            return Ok(AgentState {
                loaded: false,
                pid: None,
            });
        }
        let (status, stdout, _) = capture(
            Command::new("launchctl").args(["print", &self.target(label)]),
            LAUNCHCTL_TIMEOUT,
        )?;
        if !status.success() {
            return Ok(AgentState {
                loaded: false,
                pid: None,
            });
        }
        Ok(AgentState {
            loaded: true,
            pid: print_pid(&stdout),
        })
    }

    /// Ask launchd to remove the service; it returns at once and the
    /// process gets SIGTERM. `Ok(false)` when nothing was loaded.
    fn bootout(&self, label: &str) -> Result<bool> {
        if !HAS_LAUNCHD {
            return Ok(false);
        }
        let target = self.target(label);
        let (status, _, stderr) = capture(
            Command::new("launchctl").args(["bootout", &target]),
            LAUNCHCTL_TIMEOUT,
        )?;
        if status.success() {
            return Ok(true);
        }
        // 3 (ESRCH) and 113 are launchctl's "no such service" replies.
        if matches!(status.code(), Some(3) | Some(113))
            || stderr.contains("Could not find service")
            || stderr.contains("No such process")
        {
            return Ok(false);
        }
        anyhow::bail!("launchctl bootout {target} failed ({status}): {stderr}");
    }

    /// Wait until the service is gone, forcing it with SIGKILL once the
    /// timeout passes (a hung supervisor would otherwise hold the label
    /// for launchd's `ExitTimeOut`).
    fn wait_unloaded(&self, label: &str) -> Result<()> {
        let started = Instant::now();
        let mut killed = false;
        loop {
            if !self.state(label)?.loaded {
                return Ok(());
            }
            if started.elapsed() >= REPLACE_TIMEOUT * if killed { 2 } else { 1 } {
                ensure!(
                    !killed,
                    "agent {label} did not unload within {}s even after SIGKILL",
                    2 * REPLACE_TIMEOUT.as_secs()
                );
                let _ = capture(
                    Command::new("launchctl").args(["kill", "SIGKILL", &self.target(label)]),
                    LAUNCHCTL_TIMEOUT,
                );
                killed = true;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The `pid = N` line of `launchctl print`.
pub fn print_pid(listing: &str) -> Option<u32> {
    listing
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

impl LaunchAgent for Launchctl {
    fn install(&self, label: &str, path: &Path, contents: &str) -> Result<()> {
        ensure!(
            path.is_absolute(),
            "LaunchAgent path {} must be absolute (HOME is unset?)",
            path.display()
        );
        ensure!(
            HAS_LAUNCHD,
            "launchd mode needs macOS: this host ({}) has no launchd to keep the supervisor resident",
            std::env::consts::OS
        );
        let dir = path.parent().context("LaunchAgent path has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        // The definition carries the user's PATH: readable by its owner only.
        fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
        // A definition already loaded keeps its old arguments until reloaded,
        // and launchd refuses a bootstrap while the old service is exiting.
        if self.bootout(label)? {
            self.wait_unloaded(label)?;
        }
        let (status, _, stderr) = capture(
            Command::new("launchctl")
                .args(["bootstrap", &self.domain()])
                .arg(path),
            LAUNCHCTL_TIMEOUT,
        )?;
        ensure!(
            status.success(),
            "launchctl bootstrap {} {} failed ({status}): {stderr}",
            self.domain(),
            path.display()
        );
        Ok(())
    }

    fn uninstall(&self, label: &str, path: &Path) -> Result<AgentState> {
        let state = self.state(label)?;
        if state.loaded {
            self.bootout(label)?;
        }
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_lists_every_key_launchd_needs() {
        let spec = LaunchAgentSpec {
            label: "com.dagq.abc".into(),
            plist: "/home/u/Library/LaunchAgents/com.dagq.abc.plist".into(),
            program_arguments: vec![
                "/bin/dagq".into(),
                "--db".into(),
                "/data/q/queue.db".into(),
                "supervise".into(),
                "--parallel".into(),
                "3".into(),
                "--log-dir".into(),
                "/data/q/logs".into(),
            ],
            working_directory: "/repo & co".into(),
            environment: SupervisorEnvironment {
                path: "/usr/bin:/home/u/.local/bin".into(),
                config_home: None,
            },
            log: "/data/q/logs/launchd.log".into(),
        };
        let xml = spec.xml();
        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains("<key>Label</key>\n\t<string>com.dagq.abc</string>"));
        assert!(xml.contains(
            "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/bin/dagq</string>\n\t\t<string>--db</string>"
        ));
        assert!(xml.contains("<string>--parallel</string>\n\t\t<string>3</string>\n\t\t<string>--log-dir</string>\n\t\t<string>/data/q/logs</string>\n\t</array>"));
        assert!(xml.contains("<key>WorkingDirectory</key>\n\t<string>/repo &amp; co</string>"));
        assert!(xml.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin:/home/u/.local/bin</string>\n\t</dict>"
        ));
        assert!(xml.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(xml.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(xml.contains("<key>ExitTimeOut</key>\n\t<integer>86400</integer>"));
        assert!(
            xml.contains("<key>StandardOutPath</key>\n\t<string>/data/q/logs/launchd.log</string>")
        );
        assert!(
            xml.contains(
                "<key>StandardErrorPath</key>\n\t<string>/data/q/logs/launchd.log</string>"
            )
        );
        assert!(xml.ends_with("</dict>\n</plist>\n"));
        // The supervisor calls no cmux, so no cmux variable is given.
        assert!(!xml.contains("CMUX_"));

        // An exported XDG_CONFIG_HOME goes into the same dict, escaped like
        // the rest.
        let with_config_home = LaunchAgentSpec {
            environment: SupervisorEnvironment {
                path: "/usr/bin".into(),
                config_home: Some("/home/u/.cfg&<>".into()),
            },
            ..spec
        }
        .xml();
        assert!(
            with_config_home.contains(
                "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/usr/bin</string>\n\t\t<key>XDG_CONFIG_HOME</key>\n\t\t<string>/home/u/.cfg&amp;&lt;&gt;</string>\n\t</dict>"
            ),
            "{with_config_home}"
        );
    }

    #[test]
    fn uninstall_tolerates_a_missing_plist_but_not_a_relative_install_path() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: getuid has no preconditions.
        let launchctl = Launchctl {
            uid: unsafe { libc::getuid() },
        };
        let error = launchctl
            .install("x", Path::new("relative.plist"), "")
            .unwrap_err();
        assert!(format!("{error:#}").contains("must be absolute"));
        // Nothing is loaded under a label that was never bootstrapped, and a
        // plist that is not there is not an error either.
        let missing = dir.path().join("com.dagq.missing.plist");
        assert_eq!(
            launchctl.uninstall("com.dagq.missing", &missing).unwrap(),
            AgentState {
                loaded: false,
                pid: None
            }
        );
        assert!(!launchctl.bootout("com.dagq.missing").unwrap());
    }

    /// Off macOS `up` names what is missing instead of failing to start
    /// `launchctl`, and writes no definition.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn install_off_macos_says_launchd_mode_needs_macos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents/com.dagq.x.plist");
        let error = Launchctl { uid: 0 }
            .install("com.dagq.x", &path, "")
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("launchd mode needs macOS"),
            "{error:#}"
        );
        assert!(!path.exists());
    }

    #[test]
    fn print_pid_reads_the_pid_line() {
        let listing = "gui/501/com.dagq.x = {\n\tactive count = 1\n\tpath = /p\n\tstate = running\n\n\tpid = 4213\n\tprogram = /bin\n}";
        assert_eq!(print_pid(listing), Some(4213));
        assert_eq!(
            print_pid("gui/501/x = {\n\tstate = spawn scheduled\n}"),
            None
        );
    }
}
