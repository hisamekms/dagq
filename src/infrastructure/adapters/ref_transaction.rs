//! A ref write staged in a `git update-ref --stdin` transaction
//! ([`crate::application::Repository::prepare_ref`]): `start`, then
//! `update`, `create` or `delete` with the value the ref must hold now,
//! then `prepare` lock the ref and check the write can be done; `commit`
//! does it and `abort` (or the end of git's input without a commit) leaves
//! the ref as it was. Git holds the lock while its process waits for the
//! next command, so the handle keeps that process and its input until it is
//! settled. A git that does not end once its input is closed is sent
//! `SIGTERM`, on which it lets go of the lock, before it is killed.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

use super::{GitRepository, OUTPUT_TIMEOUT, output_file, read_back};
use crate::application::PreparedRef;

impl GitRepository {
    pub fn prepare_ref(
        &self,
        name: &str,
        value: Option<&str>,
        old: Option<&str>,
    ) -> Result<GitPreparedRef> {
        let write = match (value, old) {
            (Some(value), Some(old)) => format!("update {name} {value} {old}"),
            (Some(value), None) => format!("create {name} {value}"),
            (None, Some(old)) => format!("delete {name} {old}"),
            (None, None) => bail!("no write of {name} to prepare: it is to stay absent"),
        };
        let stderr = output_file()?;
        let mut child = self
            .command()
            .arg("-C")
            .arg(&self.root)
            .args(["update-ref", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.try_clone()?)
            .spawn()
            .context("start git update-ref --stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("git update-ref has no stdout")?;
        let (lines, answers) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        let mut prepared = GitPreparedRef {
            name: name.to_owned(),
            stdin: child.stdin.take(),
            child,
            answers,
            stderr,
        };
        prepared.send(&format!("start\n{write}\nprepare\n"))?;
        for step in ["start", "prepare"] {
            prepared.answered(step)?;
        }
        Ok(prepared)
    }
}

/// How long a git sent `SIGTERM` has to let go of its lock and end before
/// it is killed.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// The `git update-ref --stdin` process that holds the lock of a prepared
/// ref write, and what it answers.
pub struct GitPreparedRef {
    name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    answers: mpsc::Receiver<String>,
    stderr: std::fs::File,
}

impl GitPreparedRef {
    fn send(&mut self, commands: &str) -> Result<()> {
        let sent = self
            .stdin
            .as_mut()
            .context("git update-ref's input is closed")
            .and_then(|stdin| {
                stdin.write_all(commands.as_bytes())?;
                stdin.flush()?;
                Ok(())
            });
        sent.map_err(|error| self.failed(&format!("{error:#}")))
    }

    /// Wait for git to say `<step>: ok`, within [`OUTPUT_TIMEOUT`].
    fn answered(&mut self, step: &str) -> Result<()> {
        let deadline = Instant::now() + OUTPUT_TIMEOUT;
        let expected = format!("{step}: ok");
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.answers.recv_timeout(left) {
                Ok(line) if line == expected => return Ok(()),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(self.failed(&format!("no `{expected}` in {OUTPUT_TIMEOUT:?}")));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(self.failed(&format!("it ended before `{expected}`")));
                }
            }
        }
    }

    /// The error of a step that failed, with what git said: its input is
    /// closed, which aborts the transaction, and the process waited for.
    fn failed(&mut self, why: &str) -> anyhow::Error {
        self.close(Duration::from_secs(5));
        let said = self
            .stderr
            .try_clone()
            .and_then(read_back)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .unwrap_or_default();
        anyhow::anyhow!("git update-ref of {} failed: {why}: {said}", self.name)
    }

    /// Close git's input and wait for it to end within `timeout`; past
    /// that, send it `SIGTERM`, on which git removes its lock, and kill it
    /// only if it has not ended [`TERM_GRACE`] later. `None` when it did not
    /// end within `timeout`.
    fn close(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        drop(self.stdin.take());
        if let Some(status) = self.ended_within(timeout) {
            return Some(status);
        }
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: kill(2) takes no pointer.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        if self.ended_within(TERM_GRACE).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        None
    }

    fn ended_within(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return None,
            }
        }
    }

    fn finish(mut self, command: &str) -> Result<()> {
        self.send(&format!("{command}\n"))?;
        if command == "commit" {
            self.answered(command)?;
        }
        match self.close(OUTPUT_TIMEOUT) {
            Some(status) if status.success() => Ok(()),
            Some(status) => Err(self.failed(&format!("it ended with {status}"))),
            None => bail!(
                "git update-ref of {} did not end in {OUTPUT_TIMEOUT:?}",
                self.name
            ),
        }
    }
}

impl PreparedRef for GitPreparedRef {
    fn commit(self: Box<Self>) -> Result<()> {
        self.finish("commit")
    }

    fn abort(self: Box<Self>) -> Result<()> {
        self.finish("abort")
    }
}

impl Drop for GitPreparedRef {
    /// Unsettled, the end of git's input aborts the transaction.
    fn drop(&mut self) {
        if self.stdin.is_some() {
            self.close(Duration::from_secs(5));
        }
    }
}
