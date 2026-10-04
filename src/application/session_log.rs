//! `run log` / `planner log` (ADR-t1404-1 decision 6): the output a
//! session wrapper started in the background writes instead of a
//! terminal (its `[dagq]` summary of each turn: the start, the agent's
//! text, its tools and the outcome), read by a person or the inbox at a
//! person's word. The target is named by its run (or task) or planner id
//! and its log is looked up in the queue: a run's from its last
//! `wrapper_launched`, a planner's in its directory. The log of an ended
//! run or a closed planner is read as well; `--follow` prints what the
//! wrapper appends until it ends. The caller authorizes the command first
//! (`screen.read`, as `run screen`; `docs/design/authorization.md`).

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::planner::{PLANNER_SESSION_LOG, planner_dir};
use super::screen::{RunTarget, resolve_run};
use super::{ProcessControl, Queue, RunFiles};
use crate::domain::{
    PlannerId,
    background_wrapper::{BackgroundSession, is_background, last_background_session},
    turn::turns_dir,
};

/// How often `--follow` looks for what the wrapper appended.
pub const FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

/// The log of the background session a run or a planner started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLog {
    pub session: BackgroundSession,
}

impl SessionLog {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.session.log)
    }
}

/// The log of the background session the run `target` names (a run, or a
/// task's latest run) started last, its ended one included. A run whose
/// sessions all ran in a workspace has none.
pub fn run_log(queue: &mut dyn Queue, target: &RunTarget) -> Result<SessionLog> {
    let run = resolve_run(queue, target)?;
    match last_background_session(&queue.run_events(run.id())?) {
        Some(session) => Ok(SessionLog { session }),
        None if run.workspace_id().is_some() => bail!(
            "run {} started no session in the background: its session runs in a workspace (read it with `run screen`; a headless run's turns are in {}/turns)",
            run.id(),
            run.run_dir().unwrap_or("its run directory")
        ),
        None => bail!("run {} has started no session yet", run.id()),
    }
}

/// The log of `planner`'s session when it runs in the background
/// (`session.log` of its directory under `planners_dir`), a closed
/// planner's included. A planner in a workspace has none.
pub fn planner_log(
    queue: &dyn Queue,
    planners_dir: &Path,
    planner: PlannerId,
) -> Result<SessionLog> {
    let session = queue.planner(planner)?;
    let dir = planner_dir(planners_dir, planner);
    match session.workspace_id.as_deref() {
        Some(handle) if is_background(handle) => {
            let log = dir.join(PLANNER_SESSION_LOG).display().to_string();
            match BackgroundSession::of(handle, log) {
                Some(session) => Ok(SessionLog { session }),
                None => bail!("planner {planner} records {handle:?}, which names no process"),
            }
        }
        Some(_) => bail!(
            "planner {planner} runs in a workspace, not the background: read it with `planner screen` (a headless planner's turns are in {})",
            turns_dir(&dir).display()
        ),
        None => bail!("planner {planner} has started no session yet"),
    }
}

/// The last `lines` lines of `bytes` (all of them with `None`).
fn last_lines(bytes: &[u8], lines: Option<usize>) -> &[u8] {
    let Some(lines) = lines else {
        return bytes;
    };
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(lines.saturating_sub(1))
        .map_or(0, |(at, _)| at + 1);
    if lines == 0 { &[] } else { &bytes[start..] }
}

/// What printing a log reads: the files (a run's log is in the
/// worker-writable run dir, read without following links) and the
/// processes, to tell whether the wrapper still runs.
pub struct LogPorts<'a> {
    pub files: &'a dyn RunFiles,
    pub processes: &'a dyn ProcessControl,
}

/// Whether the wrapper of `log` still runs: its pid shows its start.
pub fn wrapper_runs(processes: &dyn ProcessControl, log: &SessionLog) -> bool {
    let wrapper = log.session.wrapper();
    wrapper.is(
        wrapper.pid,
        processes.start_identity(wrapper.pid).as_deref(),
    )
}

/// The first window a `--lines` read takes from the end of the log, doubled
/// until it holds the lines or the whole log.
const TAIL_WINDOW: u64 = 64 * 1024;

/// The last `lines` lines of the log of `size` bytes, read from its end
/// only: a large log is not read whole for a few lines.
fn tail(files: &dyn RunFiles, path: &Path, size: u64, lines: usize) -> Result<Vec<u8>> {
    let mut window = TAIL_WINDOW;
    loop {
        let bytes = files
            .read_tail(path, window.min(size))
            .with_context(|| format!("read {}", path.display()))?;
        let whole = window >= size;
        let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
        // One more line end than lines: the first line read may be cut.
        if whole || newlines > lines {
            return Ok(last_lines(&bytes, Some(lines)).to_vec());
        }
        window = window.saturating_mul(2);
    }
}

/// The size of the log: none while it is not there.
fn size(files: &dyn RunFiles, path: &Path) -> Result<Option<u64>> {
    match files.size(path) {
        Ok(size) => Ok(Some(size)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// The bytes of the log from `offset` on: none while it is not there.
fn read_from(files: &dyn RunFiles, path: &Path, offset: u64) -> Result<Vec<u8>> {
    match files.read_from(path, offset) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Print `log` to `out`: its last `lines` lines (all with `None`) and,
/// with `follow`, what the wrapper appends, looked for every `follow`
/// until the wrapper has ended (what it wrote last read after that).
pub fn print(
    ports: &LogPorts<'_>,
    log: &SessionLog,
    lines: Option<usize>,
    follow: Option<Duration>,
    out: &mut dyn Write,
) -> Result<()> {
    let path = log.path();
    let mut runs = follow.is_some() && wrapper_runs(ports.processes, log);
    let mut offset = match (size(ports.files, &path)?, lines) {
        (None, _) if !runs => bail!("the log {} is not there", path.display()),
        (None, _) => 0,
        (Some(size), Some(lines)) => {
            out.write_all(&tail(ports.files, &path, size, lines)?)?;
            size
        }
        (Some(_), None) => {
            let bytes = read_from(ports.files, &path, 0)?;
            out.write_all(&bytes)?;
            bytes.len() as u64
        }
    };
    out.flush()?;
    while let (true, Some(interval)) = (runs, follow) {
        std::thread::sleep(interval);
        // Judged before the read, so the read after the end takes all.
        runs = wrapper_runs(ports.processes, log);
        let more = read_from(ports.files, &path, offset)?;
        offset += more.len() as u64;
        out.write_all(&more)?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_lines_are_cut_at_line_ends() {
        let text = b"a\nb\nc\n";
        assert_eq!(last_lines(text, None), text);
        assert_eq!(last_lines(text, Some(2)), b"b\nc\n");
        assert_eq!(last_lines(text, Some(3)), text);
        assert_eq!(last_lines(text, Some(9)), text);
        assert_eq!(last_lines(text, Some(0)), b"");
        assert_eq!(last_lines(b"a\nb", Some(1)), b"b");
        assert_eq!(last_lines(b"", Some(1)), b"");
    }

    #[test]
    fn the_last_lines_of_a_large_log_are_read_from_its_end() {
        use crate::application::memory_files::MemoryFiles;
        let files = MemoryFiles::default();
        let path = Path::new("/q/runs/r/session.log");
        // Lines longer than the first window, so it is doubled.
        let line = "x".repeat(50 * 1024);
        let text: String = (0..6).map(|n| format!("{n}{line}\n")).collect();
        files.put(path, std::time::SystemTime::UNIX_EPOCH, &text);
        let size = files.size(path).unwrap();
        assert_eq!(size, text.len() as u64);
        let last = tail(&files, path, size, 2).unwrap();
        assert_eq!(last, format!("4{line}\n5{line}\n").into_bytes());
        // More lines than the log has: all of it.
        assert_eq!(tail(&files, path, size, 99).unwrap(), text.as_bytes());
        assert_eq!(tail(&files, path, size, 0).unwrap(), b"");
    }
}
