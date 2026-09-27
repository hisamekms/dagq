//! Drawing the near-term dependency diagram with the host's d2 and its
//! TALA layout plugin (ADR-0077 decisions 4 and 5): both are resolved on
//! PATH (the supervisor's, fixed by `up`), the d2 source goes to `d2
//! --layout=tala - -` on stdin, and the SVG comes back on stdout. A missing
//! tool, a failure or a run past the timeout is an error with the reason;
//! no SVG is made then.
use std::{
    ffi::OsStr,
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use serde::Serialize;

/// The d2 executable's name on PATH.
pub const D2: &str = "d2";
/// The TALA layout plugin d2 looks for on PATH.
pub const TALA: &str = "d2plugin-tala";
/// How long one drawing may take before it is stopped.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(60);
/// How much of d2's stderr an error keeps (the end of it).
const STDERR_KEPT: usize = 2000;

/// Where a tool resolves on PATH: the file found, and what it links to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub path: PathBuf,
    /// The canonical file, when it differs from `path` (a link, such as
    /// `~/.local/bin/d2` to mise's shim).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<PathBuf>,
}

/// The first executable file `name` on `path_var`.
pub fn find(name: &str, path_var: Option<&OsStr>) -> Option<Resolved> {
    use std::os::unix::fs::PermissionsExt;
    let path_var = path_var?;
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .map(|path| {
            let resolved = path.canonicalize().ok().filter(|real| *real != path);
            Resolved { path, resolved }
        })
}

/// d2 and TALA as `doctor` reports them: each one's resolution, and
/// `error` naming what is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tools {
    pub d2: Option<Resolved>,
    pub tala: Option<Resolved>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Tools {
    pub fn on(path_var: Option<&OsStr>) -> Self {
        let d2 = find(D2, path_var);
        let tala = find(TALA, path_var);
        let missing: Vec<&str> = [(D2, d2.is_none()), (TALA, tala.is_none())]
            .into_iter()
            .filter_map(|(name, missing)| missing.then_some(name))
            .collect();
        let error = (!missing.is_empty()).then(|| {
            format!(
                "{} not found on PATH: install it with mise and link it into ~/.local/bin (ADR-0077)",
                missing.join(" and ")
            )
        });
        Self { d2, tala, error }
    }

    /// d2's path, or the reason it cannot draw.
    fn d2(&self) -> Result<&Path> {
        match (&self.d2, &self.error) {
            (Some(d2), None) => Ok(&d2.path),
            (_, Some(error)) => bail!("cannot draw the dependency diagram: {error}"),
            (None, None) => unreachable!("a missing d2 is an error"),
        }
    }
}

/// Draw `source` with `d2 --layout=tala` from `path_var`'s tools; the
/// SVG, or why there is none.
pub fn render_svg(source: &str, path_var: Option<&OsStr>, timeout: Duration) -> Result<String> {
    let tools = Tools::on(path_var);
    let d2 = tools.d2()?;
    let mut command = Command::new(d2);
    command
        .args(["--layout=tala", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(path_var) = path_var {
        command.env("PATH", path_var);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => bail!("could not start {}: {error}", d2.display()),
    };
    // Written and read by threads: a d2 that stops reading or writing must
    // not hold this one past the timeout.
    if let Some(mut stdin) = child.stdin.take() {
        let bytes = source.as_bytes().to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        let (sent, received) = mpsc::channel();
        if let Some(mut pipe) = pipe {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                let _ = sent.send(bytes);
            });
        }
        received
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let stop = |child: &mut std::process::Child| {
        if let Ok(pgid) = i32::try_from(child.id()) {
            // SAFETY: kill(2) on the group this d2 leads.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = child.wait();
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop(&mut child);
                bail!("could not wait for d2: {error}");
            }
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            bail!("d2 --layout=tala did not finish within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let grace = Duration::from_secs(5);
    let stdout = stdout.recv_timeout(grace).unwrap_or_default();
    let stderr = stderr.recv_timeout(grace).unwrap_or_default();
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let tail: String = {
            let trimmed = stderr.trim();
            let skip = trimmed.chars().count().saturating_sub(STDERR_KEPT);
            trimmed.chars().skip(skip).collect()
        };
        bail!(
            "d2 --layout=tala failed ({}): {tail}",
            status.code().map_or_else(
                || "killed by a signal".to_owned(),
                |code| format!("exit {code}")
            )
        );
    }
    let svg = String::from_utf8(stdout)
        .map_err(|_| anyhow::anyhow!("d2 --layout=tala wrote output that is not UTF-8"))?;
    if !svg.contains("<svg") {
        bail!("d2 --layout=tala wrote no SVG");
    }
    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tool(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn path_of(dir: &Path) -> std::ffi::OsString {
        std::env::join_paths([dir, Path::new("/usr/bin"), Path::new("/bin")]).unwrap()
    }

    #[test]
    fn names_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_of(dir.path());
        let tools = Tools::on(Some(&path));
        assert!(tools.d2.is_none() && tools.tala.is_none());
        assert!(
            tools
                .error
                .unwrap()
                .starts_with("d2 and d2plugin-tala not found")
        );
        tool(dir.path(), D2, "exit 0");
        // A file that is not executable is not a tool.
        std::fs::write(dir.path().join(TALA), "").unwrap();
        let tools = Tools::on(Some(&path));
        assert_eq!(tools.d2.unwrap().path, dir.path().join(D2));
        assert!(tools.error.unwrap().starts_with("d2plugin-tala not found"));
        let error = render_svg("a", Some(&path), RENDER_TIMEOUT).unwrap_err();
        assert!(format!("{error:#}").contains("cannot draw"));
        assert!(Tools::on(None).error.is_some());
    }

    #[test]
    fn draws_with_the_tools_and_reports_their_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_of(dir.path());
        tool(dir.path(), TALA, "exit 0");
        tool(
            dir.path(),
            D2,
            "[ \"$1\" = --layout=tala ] || exit 9\nprintf '<svg>'; cat; printf '</svg>'",
        );
        let link = dir.path().join("linked");
        std::fs::create_dir(&link).unwrap();
        std::os::unix::fs::symlink(dir.path().join(TALA), link.join(TALA)).unwrap();
        let linked = find(TALA, Some(link.as_os_str())).unwrap();
        assert_eq!(
            linked.resolved.unwrap(),
            dir.path().join(TALA).canonicalize().unwrap()
        );
        assert_eq!(
            render_svg("x -> y", Some(&path), RENDER_TIMEOUT).unwrap(),
            "<svg>x -> y</svg>"
        );
        tool(
            dir.path(),
            D2,
            "cat >/dev/null; echo 'bad layout' >&2; exit 3",
        );
        let error = format!(
            "{:#}",
            render_svg("x", Some(&path), RENDER_TIMEOUT).unwrap_err()
        );
        assert!(
            error.contains("exit 3") && error.contains("bad layout"),
            "{error}"
        );
        tool(dir.path(), D2, "cat >/dev/null; echo nothing");
        let error = format!(
            "{:#}",
            render_svg("x", Some(&path), RENDER_TIMEOUT).unwrap_err()
        );
        assert!(error.contains("no SVG"), "{error}");
        tool(dir.path(), D2, "exec sleep 30");
        let started = Instant::now();
        let error = format!(
            "{:#}",
            render_svg("x", Some(&path), Duration::from_millis(200)).unwrap_err()
        );
        assert!(error.contains("did not finish within"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
