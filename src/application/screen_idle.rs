//! Idleness inferred from the screen (ADR-t803-1): the agent's idle marker
//! (ADR-0016) stays the signal a session's end of turn is known by, and
//! only while it is missing, or older than the session's last input, does
//! the screen stand in for it. The session is taken for idle once cmux's
//! capture shows its input box ready ([`AgentSignals::input_ready`]), no
//! work ([`AgentSignals::working`]) and no dialog
//! ([`AgentSignals::detect_prompt`]), with the same transcript, on two
//! captures or more spaced over `[stall].screen_idle_secs`. A screen cmux
//! cannot read infers nothing. Each capture also reads whether the screen
//! shows background work the agent keeps running
//! ([`AgentSignals::screen_background`]): a span keeps one reading, and the
//! idle it infers carries it, as the marker's `background_running` would.
//!
//! The supervisor keeps the captures in memory ([`Spans`],
//! [`ScreenIdle::Record`]) and copies them to [`SCREEN_IDLE_FILE`] next to
//! the idle marker as it can; a read-only command judges from that copy
//! without writing ([`ScreenIdle::Peek`]). Nothing
//! here knows the provider or the kind of session: planners and the worker's
//! sessions (its first, resumed and revised ones) use it.

use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{AgentSignals, RunFiles, WorkspaceBackend, unix_seconds};

/// The captures of an idle-looking screen, next to the idle marker.
pub const SCREEN_IDLE_FILE: &str = "screen-idle.json";
/// The stamp the supervisor leaves next to the idle marker each time it
/// types a text into the session: an input the agent's own hook may have
/// failed to record.
pub const SUPERVISOR_INPUT_FILE: &str = "supervisor-input.json";
/// The debug log of a run's resumed session, next to its idle marker
/// (the first session's is the run's log path).
pub const RESUME_DEBUG_LOG: &str = "claude-resume.log";
/// How much of the end of the agent's debug log is read for a failed hook.
const DEBUG_LOG_TAIL_BYTES: usize = 64 * 1024;
/// How much of a failed hook's log line an event carries.
const HOOK_ERROR_EXCERPT_CHARS: usize = 300;

/// Whether the captures are kept (the supervisor, in `spans` and in
/// [`SCREEN_IDLE_FILE`]) or only read (a read-only command, which judges
/// as the next capture would).
#[derive(Clone, Copy)]
pub enum ScreenIdle<'a> {
    Record(&'a Spans),
    Peek,
}

/// The spans a supervisor keeps in memory, by idle marker: the file is
/// only their copy for the read-only commands and the next process, so a
/// full disk (the case the inference is for) loses no capture.
#[derive(Default)]
pub struct Spans(std::sync::Mutex<std::collections::HashMap<PathBuf, Observation>>);

impl Spans {
    fn get(&self, idle_marker: &Path) -> Option<Observation> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(idle_marker)
            .copied()
    }

    fn set(&self, idle_marker: &Path, span: Option<Observation>) {
        let mut spans = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match span {
            Some(span) => spans.insert(idle_marker.to_owned(), span),
            None => spans.remove(idle_marker),
        };
    }

    /// Note that the inference of the span of `idle_marker` is recorded,
    /// so the span records it once; its file is updated as it can be.
    pub fn mark_recorded(&self, files: &dyn RunFiles, idle_marker: &Path) {
        let Some(span) = self
            .get(idle_marker)
            .or_else(|| read_observation(files, idle_marker))
            .filter(|span| !span.recorded)
        else {
            return;
        };
        let span = Observation {
            recorded: true,
            ..span
        };
        self.set(idle_marker, Some(span));
        if let Err(error) = write_observation(files, idle_marker, Some(&span)) {
            tracing::warn!(error = %error, "the screen's idle span next to {} could not be noted recorded: {error}", idle_marker.display());
        }
    }
}

/// What one capture of the screen showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenLook {
    /// Input box ready, no work and no dialog, with this transcript, and
    /// background work shown running or not (`None`: the screen does not
    /// tell).
    Idle {
        transcript: u64,
        background: Option<bool>,
    },
    /// At work, a dialog or no input box.
    Busy,
    /// cmux could not read it.
    Unreadable,
}

/// Why the idle marker could not tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerState {
    /// The hook never wrote one.
    Missing,
    /// Written before the session's last input.
    Stale,
}

impl MarkerState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Stale => "stale",
        }
    }
}

/// The span of idle-looking captures [`SCREEN_IDLE_FILE`] keeps. Times are
/// Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// The first capture of the span.
    pub first_seen: i64,
    /// The latest capture of the span.
    pub last_seen: i64,
    /// The captures of the span, each at a later second than the one before.
    pub captures: u32,
    /// The fingerprint of the transcript every capture of the span showed.
    pub transcript: u64,
    /// Whether every capture of the span showed background work running
    /// (`None`: the screen does not tell).
    #[serde(default)]
    pub background: Option<bool>,
    /// Whether the span's inference was recorded as an event.
    #[serde(default)]
    pub recorded: bool,
}

/// An idle inferred from the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Inference {
    /// Always `screen`: where the idleness was read.
    pub source: &'static str,
    /// Why the marker did not tell.
    pub marker: MarkerState,
    /// The first capture of the span: when the session went idle.
    pub since: i64,
    /// The seconds the span covers.
    pub observed_secs: i64,
    pub captures: u32,
    /// Background work the screen showed running over the span; `None`
    /// from a provider whose screen does not tell.
    pub background_running: Option<bool>,
    /// Whether the span's inference is not recorded yet.
    #[serde(skip)]
    pub unrecorded: bool,
}

/// Read the screen of `workspace` for the inference.
pub fn look(
    cmux: &dyn WorkspaceBackend,
    signals: &dyn AgentSignals,
    workspace: &str,
) -> ScreenLook {
    match cmux.capture(workspace) {
        Ok(screen) => look_of(signals, &screen),
        Err(_) => ScreenLook::Unreadable,
    }
}

/// What `screen` shows for the inference.
pub fn look_of(signals: &dyn AgentSignals, screen: &str) -> ScreenLook {
    if signals.input_ready(screen)
        && !signals.working(screen)
        && signals.detect_prompt(screen).is_none()
    {
        ScreenLook::Idle {
            transcript: fingerprint(&signals.transcript(screen)),
            background: signals.screen_background(screen),
        }
    } else {
        ScreenLook::Busy
    }
}

fn fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The span after a capture at `now` that showed `look`, from `previous`:
/// an idle look extends a span that began after `last_input` with the same
/// transcript and the same background work and begins a new one otherwise
/// (background work that ends starts the idle over); a busy look ends the
/// span;
/// an unreadable one leaves it as it was.
pub fn observe(
    previous: Option<Observation>,
    now: i64,
    look: ScreenLook,
    last_input: i64,
) -> Option<Observation> {
    match look {
        ScreenLook::Unreadable => previous,
        ScreenLook::Busy => None,
        ScreenLook::Idle {
            transcript,
            background,
        } => Some(match previous {
            Some(span)
                if span.first_seen > last_input
                    && span.transcript == transcript
                    && span.background == background =>
            {
                if now > span.last_seen {
                    Observation {
                        last_seen: now,
                        captures: span.captures.saturating_add(1),
                        ..span
                    }
                } else {
                    span
                }
            }
            _ => Observation {
                first_seen: now,
                last_seen: now,
                captures: 1,
                transcript,
                background,
                recorded: false,
            },
        }),
    }
}

impl Observation {
    /// Whether the span shows the session idle: two captures or more,
    /// `threshold` seconds apart or more.
    pub fn idle(&self, threshold: i64) -> bool {
        self.captures >= 2 && self.last_seen - self.first_seen >= threshold
    }
}

/// The file of the captures next to `idle_marker`.
pub fn observation_path(idle_marker: &Path) -> PathBuf {
    idle_marker.with_file_name(SCREEN_IDLE_FILE)
}

/// The supervisor's input stamp next to `idle_marker`.
pub fn supervisor_input_path(idle_marker: &Path) -> PathBuf {
    idle_marker.with_file_name(SUPERVISOR_INPUT_FILE)
}

/// Leave the stamp of a text the supervisor typed into the session whose
/// idle marker is `idle_marker`.
pub fn record_supervisor_input(files: &dyn RunFiles, idle_marker: &Path) -> io::Result<()> {
    files.write(
        &supervisor_input_path(idle_marker),
        format!("{{\"at\":{}}}\n", unix_seconds(files.now())).as_bytes(),
    )
}

/// The span [`SCREEN_IDLE_FILE`] keeps next to `idle_marker`; a file that
/// cannot be read keeps none.
pub fn read_observation(files: &dyn RunFiles, idle_marker: &Path) -> Option<Observation> {
    let bytes = files.read(&observation_path(idle_marker)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_observation(
    files: &dyn RunFiles,
    idle_marker: &Path,
    span: Option<&Observation>,
) -> io::Result<()> {
    let path = observation_path(idle_marker);
    match span {
        Some(span) => files.write(&path, &serde_json::to_vec(span)?),
        None => match files.remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    }
}

/// What judging a session by its screen reads.
pub struct ScreenProbe<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub signals: &'a dyn AgentSignals,
    pub files: &'a dyn RunFiles,
    pub mode: ScreenIdle<'a>,
    /// `[stall].screen_idle_secs`.
    pub threshold: i64,
}

impl ScreenProbe<'_> {
    /// Capture `workspace` for the session whose idle marker is
    /// `idle_marker` and that `marker` could not judge, whose last input
    /// was at `last_input`; its idle inferred from the span the capture
    /// extends, if it is. With [`ScreenIdle::Record`] the span is kept
    /// (a file that cannot be written only loses it).
    pub fn infer(
        &self,
        workspace: &str,
        idle_marker: &Path,
        marker: MarkerState,
        now: i64,
        last_input: i64,
    ) -> Option<Inference> {
        let kept = match self.mode {
            ScreenIdle::Record(spans) => spans.get(idle_marker),
            ScreenIdle::Peek => None,
        };
        let previous = kept.or_else(|| read_observation(self.files, idle_marker));
        let look = look(self.cmux, self.signals, workspace);
        let span = observe(previous, now, look, last_input);
        if let ScreenIdle::Record(spans) = self.mode
            && span != kept
        {
            spans.set(idle_marker, span);
            if let Err(error) = write_observation(self.files, idle_marker, span.as_ref()) {
                tracing::warn!(error = %error, "the screen's idle span next to {} could not be copied: {error}", idle_marker.display());
            }
        }
        // A screen that could not be read now infers nothing, whatever the
        // span kept.
        if look == ScreenLook::Unreadable {
            return None;
        }
        span.filter(|span| span.idle(self.threshold))
            .map(|span| Inference {
                source: "screen",
                marker,
                since: span.first_seen,
                observed_secs: span.last_seen - span.first_seen,
                captures: span.captures,
                background_running: span.background,
                unrecorded: !span.recorded,
            })
    }
}

/// The last input the session whose idle marker is `idle_marker` took, as
/// its input marker and the supervisor's stamp record it, and `opened`,
/// when the session opened: the latest, as a file time.
pub fn last_input(files: &dyn RunFiles, idle_marker: &Path, opened: SystemTime) -> SystemTime {
    [
        crate::application::stats::PROMPT_SUBMIT_MARKER,
        SUPERVISOR_INPUT_FILE,
    ]
    .into_iter()
    .filter_map(|name| files.modified(&idle_marker.with_file_name(name)).ok())
    .fold(opened, SystemTime::max)
}

/// The line of the agent's debug log at `debug_log` that says its idle
/// hook failed, cut to an excerpt; `None` without one, or without a log.
pub fn hook_failure(
    files: &dyn RunFiles,
    signals: &dyn AgentSignals,
    debug_log: &Path,
) -> Option<String> {
    let bytes = files.read(debug_log).ok()?;
    let tail = &bytes[bytes.len().saturating_sub(DEBUG_LOG_TAIL_BYTES)..];
    let line = signals.idle_hook_failure(&String::from_utf8_lossy(tail))?;
    Some(line.chars().take(HOOK_ERROR_EXCERPT_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::application::{IdleHook, InputSource};
    use std::time::Duration;

    /// Screens are `ready`, `working`, `dialog` or anything else (no box),
    /// the transcript the text after `ready ` (or none); `ready shells`
    /// shows background work.
    struct Signals;

    impl AgentSignals for Signals {
        fn detect_prompt(&self, screen: &str) -> Option<&'static str> {
            (screen == "dialog").then_some("trust")
        }
        fn screen_excerpt(&self, screen: &str) -> String {
            screen.to_owned()
        }
        fn idle_hook(&self, _: &[u8]) -> IdleHook {
            IdleHook::default()
        }
        fn input_source(&self, _: &[u8]) -> InputSource {
            InputSource::Unknown
        }
        fn input_ready(&self, screen: &str) -> bool {
            screen.starts_with("ready") || screen == "working"
        }
        fn input_pending(&self, _: &str, _: &str) -> bool {
            false
        }
        fn working(&self, screen: &str) -> bool {
            screen == "working"
        }
        fn screen_background(&self, screen: &str) -> Option<bool> {
            Some(screen.ends_with("shells"))
        }
        fn idle_hook_failure(&self, log: &str) -> Option<String> {
            log.lines()
                .rev()
                .find(|line| line.contains("Hook Stop"))
                .map(str::to_owned)
        }
    }

    fn idle(transcript: &str) -> ScreenLook {
        look_of(&Signals, &format!("ready {transcript}"))
    }

    #[test]
    fn the_screen_looks_idle_only_ready_at_rest_without_a_dialog() {
        assert!(matches!(idle("a"), ScreenLook::Idle { .. }));
        assert_ne!(idle("a"), idle("b"));
        for busy in ["working", "dialog", "booting"] {
            assert_eq!(look_of(&Signals, busy), ScreenLook::Busy, "{busy}");
        }
    }

    #[test]
    fn a_span_grows_over_spaced_captures_of_the_same_transcript() {
        let first = observe(None, 100, idle("a"), 50).unwrap();
        assert_eq!((first.first_seen, first.captures), (100, 1));
        assert!(!first.idle(0));
        // A capture in the same second is not another one.
        assert_eq!(observe(Some(first), 100, idle("a"), 50), Some(first));
        let second = observe(Some(first), 130, idle("a"), 50).unwrap();
        assert_eq!(
            (second.first_seen, second.last_seen, second.captures),
            (100, 130, 2)
        );
        assert!(second.idle(30) && !second.idle(31));
        // Unreadable keeps it, busy ends it.
        assert_eq!(
            observe(Some(second), 140, ScreenLook::Unreadable, 50),
            Some(second)
        );
        assert_eq!(observe(Some(second), 140, ScreenLook::Busy, 50), None);
        // Another transcript, or an input after the span began, starts over.
        let other = observe(Some(second), 140, idle("b"), 50).unwrap();
        assert_eq!((other.first_seen, other.captures), (140, 1));
        let typed = observe(Some(second), 140, idle("a"), 110).unwrap();
        assert_eq!((typed.first_seen, typed.captures), (140, 1));
        assert!(!typed.recorded);
    }

    #[test]
    fn background_work_on_the_screen_is_kept_by_the_span_and_its_end_starts_over() {
        let shells = idle("a shells");
        assert_eq!(
            shells,
            ScreenLook::Idle {
                transcript: fingerprint("ready a shells"),
                background: Some(true),
            }
        );
        let first = observe(None, 100, shells, 50).unwrap();
        let running = observe(Some(first), 130, shells, 50).unwrap();
        assert_eq!(running.background, Some(true));
        assert!(running.idle(30));
        // The same transcript without the background work is a new span.
        let same = ScreenLook::Idle {
            transcript: fingerprint("ready a shells"),
            background: Some(false),
        };
        let done = observe(Some(running), 140, same, 50).unwrap();
        assert_eq!(
            (done.first_seen, done.captures, done.background),
            (140, 1, Some(false))
        );
        // A span written before the screen told keeps none.
        let old: Observation =
            serde_json::from_str(r#"{"first_seen":1,"last_seen":2,"captures":2,"transcript":3}"#)
                .unwrap();
        assert_eq!(old.background, None);
    }

    #[test]
    fn the_last_input_is_the_latest_of_the_hook_the_stamp_and_the_opening() {
        let files = MemoryFiles::default();
        let marker = Path::new("/p/idle.json");
        let opened = files.now() - Duration::from_secs(600);
        assert_eq!(last_input(&files, marker, opened), opened);
        let hook = files.now() - Duration::from_secs(300);
        files.put(Path::new("/p/prompt-submit.json"), hook, "{}");
        assert_eq!(last_input(&files, marker, opened), hook);
        record_supervisor_input(&files, marker).unwrap();
        assert_eq!(
            last_input(&files, marker, opened),
            files
                .modified(Path::new("/p/supervisor-input.json"))
                .unwrap()
        );
    }

    #[test]
    fn a_span_is_kept_next_to_the_marker_and_noted_recorded_once() {
        let files = MemoryFiles::default();
        let marker = Path::new("/p/idle.json");
        assert_eq!(read_observation(&files, marker), None);
        let spans = Spans::default();
        spans.mark_recorded(&files, marker);
        assert_eq!(read_observation(&files, marker), None);
        let span = observe(None, 100, idle("a"), 0).unwrap();
        write_observation(&files, marker, Some(&span)).unwrap();
        assert_eq!(read_observation(&files, marker), Some(span));
        // A span only in the file (another process's) is noted too.
        spans.mark_recorded(&files, marker);
        assert!(read_observation(&files, marker).unwrap().recorded);
        assert!(spans.get(marker).unwrap().recorded);
        // Memory keeps what the file loses.
        files.put(&observation_path(marker), files.now(), "");
        assert!(spans.get(marker).unwrap().recorded);
        spans.set(marker, None);
        assert_eq!(spans.get(marker), None);
        write_observation(&files, marker, None).unwrap();
        write_observation(&files, marker, None).unwrap();
        assert_eq!(read_observation(&files, marker), None);
        // A file that cannot be read keeps no span.
        files.put(&observation_path(marker), files.now(), "not json");
        assert_eq!(read_observation(&files, marker), None);
        assert_eq!(MarkerState::Stale.as_str(), "stale");
        assert_eq!(MarkerState::Missing.as_str(), "missing");
    }

    #[test]
    fn a_failed_hook_is_read_from_the_end_of_the_debug_log() {
        let files = MemoryFiles::default();
        let log = Path::new("/p/claude.log");
        assert_eq!(hook_failure(&files, &Signals, log), None);
        files.put(log, files.now(), "boot\nturn\n");
        assert_eq!(hook_failure(&files, &Signals, log), None);
        let long = format!(
            "{}\n2026-09-27 [ERROR] Hook Stop (Stop) error: No space left on device {}\nlater\n",
            "x".repeat(DEBUG_LOG_TAIL_BYTES),
            "y".repeat(500)
        );
        files.put(log, files.now(), &long);
        let line = hook_failure(&files, &Signals, log).unwrap();
        assert!(line.starts_with("2026-09-27 [ERROR] Hook Stop (Stop) error: No space left"));
        assert_eq!(line.chars().count(), HOOK_ERROR_EXCERPT_CHARS);
    }
}
