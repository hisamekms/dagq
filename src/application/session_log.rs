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
            "run {} started no session in the background: its session ran in a workspace an older binary opened and has no log (its turns are in {}/turns)",
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
            "planner {planner} runs in a workspace, not the background: its screen is not read any more, so read it in its workspace in your own terminal (a headless planner's turns are in {})",
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

/// How many bytes one read of the log takes: a log larger than one read
/// of the run directory allows is printed a chunk at a time.
const CHUNK: u64 = 1024 * 1024;

/// Print the log from `offset` on to `out`, `chunk` bytes per read, up to
/// its end (none while it is not there); the offset after it.
fn copy_from(
    files: &dyn RunFiles,
    path: &Path,
    mut offset: u64,
    chunk: u64,
    out: &mut dyn Write,
) -> Result<u64> {
    loop {
        let bytes = match files.read_range(path, offset, chunk) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        out.write_all(&bytes)?;
        offset += bytes.len() as u64;
        // A short read is the end the log had then.
        if (bytes.len() as u64) < chunk {
            return Ok(offset);
        }
    }
}

/// Print `log` to `out`: its last `lines` lines (all with `None`) and,
/// with `follow`, what the wrapper appends, looked for every `follow`
/// until the wrapper has ended (what it wrote last read after that). The
/// whole log and what is appended are read in chunks, so no size limits
/// what is printed.
pub fn print(
    ports: &LogPorts<'_>,
    log: &SessionLog,
    lines: Option<usize>,
    follow: Option<Duration>,
    out: &mut dyn Write,
) -> Result<()> {
    print_in_chunks(ports, log, lines, follow, CHUNK, out)
}

fn print_in_chunks(
    ports: &LogPorts<'_>,
    log: &SessionLog,
    lines: Option<usize>,
    follow: Option<Duration>,
    chunk: u64,
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
        (Some(_), None) => copy_from(ports.files, &path, 0, chunk, out)?,
    };
    out.flush()?;
    while let (true, Some(interval)) = (runs, follow) {
        std::thread::sleep(interval);
        // Judged before the read, so the read after the end takes all.
        runs = wrapper_runs(ports.processes, log);
        offset = copy_from(ports.files, &path, offset, chunk, out)?;
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

    use crate::application::memory_files::MemoryFiles;

    const LOG: &str = "/q/runs/r/session.log";

    /// A wrapper that appends `appends` to the log one by one as
    /// `--follow` looks whether it still runs, and has ended once all is
    /// appended.
    struct Appending<'a> {
        files: &'a MemoryFiles,
        appends: std::sync::Mutex<Vec<String>>,
    }

    impl ProcessControl for Appending<'_> {
        fn alive(&self, _: u32) -> bool {
            unreachable!()
        }
        fn terminate(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn kill(&self, _: u32) -> Result<()> {
            unreachable!()
        }
        fn start_identity(&self, _: u32) -> Option<String> {
            let mut appends = self.appends.lock().unwrap();
            if appends.is_empty() {
                return None;
            }
            let more = appends.remove(0);
            let path = Path::new(LOG);
            let mut text = String::from_utf8(self.files.bytes(path).unwrap()).unwrap();
            text.push_str(&more);
            self.files
                .put(path, std::time::SystemTime::UNIX_EPOCH, &text);
            Some("start".to_owned())
        }
    }

    /// Files holding `text` as the log, read at most `limit` bytes at once.
    fn bounded(limit: u64, text: &str) -> MemoryFiles {
        let files = MemoryFiles::bounded(limit);
        files.put(Path::new(LOG), std::time::SystemTime::UNIX_EPOCH, text);
        files
    }

    fn appending<'a>(files: &'a MemoryFiles, appends: &[String]) -> Appending<'a> {
        Appending {
            files,
            appends: std::sync::Mutex::new(appends.to_vec()),
        }
    }

    fn session_log() -> SessionLog {
        SessionLog {
            session: BackgroundSession::of("background:7:start", LOG.to_owned()).unwrap(),
        }
    }

    /// Numbered lines of `bytes` bytes or more, so a cut chunk or a lost
    /// one shows.
    fn numbered(from: usize, bytes: usize) -> String {
        let mut text = String::new();
        let mut n = from;
        while text.len() < bytes {
            text.push_str(&format!("line {n}\n"));
            n += 1;
        }
        text
    }

    #[test]
    fn a_log_larger_than_one_read_is_printed_whole_in_chunks() {
        let text = numbered(0, 1000);
        let files = bounded(64, &text);
        let processes = appending(&files, &[]);
        let ports = LogPorts {
            files: &files,
            processes: &processes,
        };
        let mut out = Vec::new();
        print_in_chunks(&ports, &session_log(), None, None, 64, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), text);
        // A log of exactly whole chunks ends with an empty read.
        let text = "x".repeat(128);
        let files = bounded(64, &text);
        let processes = appending(&files, &[]);
        let ports = LogPorts {
            files: &files,
            processes: &processes,
        };
        let mut out = Vec::new();
        print_in_chunks(&ports, &session_log(), None, None, 64, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), text);
        // `--lines` still prints the tail only, of a log larger than one read.
        let text = numbered(0, 1000);
        let files = bounded(64, &text);
        let processes = appending(&files, &[]);
        let ports = LogPorts {
            files: &files,
            processes: &processes,
        };
        let last = text.lines().last().unwrap();
        let mut out = Vec::new();
        print_in_chunks(&ports, &session_log(), Some(1), None, 64, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("{last}\n"));
    }

    #[test]
    fn follow_keeps_up_when_more_than_one_read_is_appended_between_polls() {
        let text = numbered(0, 300);
        let appends = [numbered(1000, 500), numbered(2000, 700)];
        let files = bounded(64, &text);
        let processes = appending(&files, &appends);
        let ports = LogPorts {
            files: &files,
            processes: &processes,
        };
        let mut out = Vec::new();
        print_in_chunks(
            &ports,
            &session_log(),
            None,
            Some(Duration::ZERO),
            64,
            &mut out,
        )
        .unwrap();
        let whole = String::from_utf8(files.bytes(Path::new(LOG)).unwrap()).unwrap();
        assert_eq!(whole, format!("{text}{}{}", appends[0], appends[1]));
        assert_eq!(String::from_utf8(out).unwrap(), whole);
    }

    #[test]
    fn a_failed_read_names_the_log() {
        let files = bounded(64, &numbered(0, 100));
        let processes = appending(&files, &[]);
        let ports = LogPorts {
            files: &files,
            processes: &processes,
        };
        let error =
            print_in_chunks(&ports, &session_log(), None, None, 65, &mut Vec::new()).unwrap_err();
        assert!(format!("{error:#}").contains(LOG), "{error:#}");
    }
}
