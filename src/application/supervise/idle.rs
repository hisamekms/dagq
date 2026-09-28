//! The idle marker a session's agent writes when it stops (ADR-0016): when
//! it was written, and what the agent's adapter read of it
//! ([`AgentSignals::idle_hook`]). The agent is idle when it finished a
//! response and left no background work running (task 147); a `/exit` sent
//! while background work runs stops at the agent's own dialog.
//!
//! Only while the marker is missing, or older than the session's last
//! input, does the screen stand in for it ([`Supervisor::session_idle`],
//! ADR-t803-1): an idle inferred there carries the background work the
//! screen showed ([`AgentSignals::screen_background`]) as the marker's
//! `background_running`, so a session that shows background work is waited
//! for as one whose marker says so, and not sent a `/exit` that would stop
//! at the agent's dialog. It lists no background task, and a provider whose
//! screen does not tell is taken for one without background work.

use super::*;
use crate::application::screen_idle::{
    self, Inference, MarkerState, RESUME_DEBUG_LOG, ScreenIdle, ScreenLook, ScreenProbe,
};
use crate::domain::EventKind;

pub(super) struct IdleMarker {
    path: PathBuf,
    modified: SystemTime,
    hook: IdleHook,
    /// Inferred from the screen rather than written by the agent's hook.
    inferred: Option<Inference>,
}

impl IdleMarker {
    /// `None` when the hook never wrote one. Time and content come from one
    /// open file, so they belong to the same write (the hook replaces the
    /// marker by a rename).
    pub(super) fn read(
        files: &dyn RunFiles,
        signals: &dyn AgentSignals,
        path: &Path,
    ) -> Result<Option<Self>> {
        let Some((modified, bytes)) = files.read_stamped(path)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            path: path.to_owned(),
            modified,
            hook: signals.idle_hook(&bytes),
            inferred: None,
        }))
    }

    /// The idle the screen showed for the marker at `path` (ADR-t803-1):
    /// written, as it were, when the screen's span began, with the
    /// background work the screen showed (none listed), and the inference
    /// in its evidence.
    fn inferred(path: &Path, inference: Inference) -> Self {
        Self {
            path: path.to_owned(),
            modified: UNIX_EPOCH + Duration::from_secs(u64::try_from(inference.since).unwrap_or(0)),
            hook: IdleHook {
                background_running: inference.background_running == Some(true),
                background_tasks: Vec::new(),
                evidence: vec![
                    ("source", json!(inference.source)),
                    ("marker", json!(inference.marker.as_str())),
                    ("observed_secs", json!(inference.observed_secs)),
                    ("captures", json!(inference.captures)),
                ],
            },
            inferred: Some(inference),
        }
    }

    /// Whether the screen showed this idle rather than the agent's hook
    /// writing it.
    pub(super) fn is_inferred(&self) -> bool {
        self.inferred.is_some()
    }

    /// When the agent wrote it.
    pub(super) fn modified(&self) -> SystemTime {
        self.modified
    }

    /// The background tasks still `running` when the agent stopped.
    pub(super) fn background_tasks(&self) -> &[BackgroundTask] {
        &self.hook.background_tasks
    }

    /// Background work the agent left running when it stopped.
    pub(super) fn background_running(&self) -> bool {
        self.hook.background_running
    }

    /// Background work left running, as evidence: for an idle the screen
    /// showed, what the screen read, `null` from a provider whose screen
    /// does not tell.
    pub(super) fn background_running_evidence(&self) -> Value {
        match self.inferred {
            Some(inference) => json!(inference.background_running),
            None => json!(self.background_running()),
        }
    }

    /// Whether the session took an input (`input`, its input marker) after
    /// the agent wrote this marker: the input started a turn that has not
    /// ended, whatever its source (a person's prompt, a text the
    /// supervisor typed, or the agent's own notice that its background
    /// work ended), so this idle is not the session's latest (task 672).
    /// An idle the screen showed began after the last input, and is never.
    pub(super) fn turn_open_after(&self, input: Option<&InputMarker>) -> bool {
        self.inferred.is_none() && input.is_some_and(|input| input.modified > self.modified)
    }

    /// Idle, by a marker written after `since`.
    pub(super) fn idle_since(&self, since: SystemTime) -> bool {
        !self.background_running() && self.modified > since
    }

    /// Evidence that the agent went idle after publishing the receipt: the
    /// marker is no older than the receipt. Markers from earlier turns (for
    /// example a question to the inbox) do not count.
    pub(super) fn idle_after_receipt(
        &self,
        files: &dyn RunFiles,
        receipt: &Path,
    ) -> Result<Option<Value>> {
        if self.background_running() {
            return Ok(None);
        }
        self.stopped_after_receipt(files, receipt)
    }

    /// Evidence that the agent stopped after publishing the receipt, idle
    /// or not (its background work still running).
    pub(super) fn stopped_after_receipt(
        &self,
        files: &dyn RunFiles,
        receipt: &Path,
    ) -> Result<Option<Value>> {
        let receipt_modified = files.modified(receipt)?;
        if self.modified < receipt_modified {
            return Ok(None);
        }
        let mut evidence = serde_json::Map::new();
        evidence.insert("marker_path".into(), json!(path_text(&self.path)?));
        evidence.insert("marker_modified".into(), json!(unix_seconds(self.modified)));
        evidence.insert(
            "receipt_modified".into(),
            json!(unix_seconds(receipt_modified)),
        );
        for (name, value) in &self.hook.evidence {
            evidence.insert((*name).into(), value.clone());
        }
        // What the screen read of background work; unknown, not none, from
        // a provider whose screen does not tell.
        evidence.insert(
            "background_running".into(),
            self.background_running_evidence(),
        );
        Ok(Some(Value::Object(evidence)))
    }
}

/// How long a worker session's screen is not captured again for the
/// inference ([`Supervisor::session_idle`]): half of
/// `[stall].screen_idle_secs`, and a minute at most, so a span reaches the
/// threshold within one more capture and a session that works is not
/// captured on every tick.
pub(super) fn probe_interval(screen_idle_secs: i64) -> Duration {
    Duration::from_secs(u64::try_from((screen_idle_secs / 2).clamp(0, 60)).unwrap_or(0))
}

/// How many [`probe_interval`]s the wait between captures grows to while
/// the screen keeps showing the session at work (task 845).
const PROBE_BACKOFF_LIMIT: u32 = 4;

/// The longest wait between two captures of a session whose screen keeps
/// showing it at work: [`PROBE_BACKOFF_LIMIT`] [`probe_interval`]s, four
/// minutes at most. A session that comes to rest is captured within it.
pub(super) fn max_probe_interval(screen_idle_secs: i64) -> Duration {
    probe_interval(screen_idle_secs) * PROBE_BACKOFF_LIMIT
}

/// The last capture of a session judged by its screen.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Probe {
    at: SystemTime,
    /// What it inferred.
    inference: Option<Inference>,
    /// The session's last input then, in Unix seconds.
    floor: i64,
    /// Whether the screen showed the session at work.
    working: bool,
    /// How long after `at` the screen is captured again, unless an input
    /// comes first.
    wait: Duration,
}

/// The last capture of each session judged by its screen, by idle marker.
/// While capture after capture shows the session at work since the same
/// input, the wait before the next one doubles from [`probe_interval`] up
/// to [`max_probe_interval`]; any other look (at rest, a dialog, no input
/// box, a screen cmux cannot read), and a new input, bring it back.
#[derive(Default)]
pub(super) struct ScreenProbes(std::sync::Mutex<HashMap<PathBuf, Probe>>);

impl ScreenProbes {
    fn get(&self, idle_marker: &Path) -> Option<Probe> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(idle_marker)
            .copied()
    }

    /// The inference of the last capture of the session of `idle_marker`
    /// while its screen is not to be captured again at `now`, the
    /// session's last input at `floor`; an input after that capture
    /// brings the wait back to `base`.
    fn kept(
        &self,
        idle_marker: &Path,
        now: SystemTime,
        floor: i64,
        base: Duration,
    ) -> Option<Option<Inference>> {
        let probe = self.get(idle_marker)?;
        let wait = if floor > probe.floor {
            probe.wait.min(base)
        } else {
            probe.wait
        };
        (now < probe.at + wait).then(|| probe.inference.filter(|inference| inference.since > floor))
    }

    /// Keep a capture at `now` that showed `look` and inferred `inference`,
    /// the session's last input at `floor`, and the wait before the next.
    #[allow(clippy::too_many_arguments)]
    fn capture(
        &self,
        idle_marker: &Path,
        now: SystemTime,
        floor: i64,
        look: ScreenLook,
        inference: Option<Inference>,
        base: Duration,
        limit: Duration,
    ) -> Duration {
        let working = look == ScreenLook::Working;
        let wait = match self.get(idle_marker) {
            Some(previous) if working && previous.working && previous.floor == floor => {
                previous.wait.saturating_mul(2).min(limit).max(base)
            }
            _ => base,
        };
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                idle_marker.to_owned(),
                Probe {
                    at: now,
                    inference,
                    floor,
                    working,
                    wait,
                },
            );
        wait
    }
}

impl Supervisor<'_> {
    /// The idle marker of the session of `run` in `workspace`, or, while
    /// it is missing or older than the session's last input (its input
    /// marker, the supervisor's stamp, or `after`, the latest input the
    /// caller knows of), the idle its screen shows (ADR-t803-1). The span
    /// the screen was first inferred idle over is recorded as
    /// `idle_inferred` once, with `phase`. Without an inference the marker
    /// is returned as it was read, however old.
    pub(super) fn session_idle(
        &self,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        after: SystemTime,
        phase: &str,
    ) -> Result<Option<IdleMarker>> {
        let marker = IdleMarker::read(&*self.files, self.signals, idle_marker)?;
        // A headless session's wrapper writes the marker when each turn
        // ends: there is no screen to infer from (ADR-t813-1).
        if headless(run) {
            return Ok(marker);
        }
        let last_input = screen_idle::last_input(&*self.files, idle_marker, after);
        let state = match &marker {
            None => MarkerState::Missing,
            Some(idle) if idle.modified() < last_input => MarkerState::Stale,
            Some(_) => return Ok(marker),
        };
        let now = self.files.now();
        let floor = unix_seconds(last_input);
        let base = probe_interval(self.stall.screen_idle_secs);
        let inference = match self.screen_probes.kept(idle_marker, now, floor, base) {
            Some(inference) => inference,
            None => {
                let (look, inference) = ScreenProbe {
                    cmux: self.cmux,
                    signals: self.signals,
                    files: &*self.files,
                    mode: ScreenIdle::Record(&self.screen_spans),
                    threshold: self.stall.screen_idle_secs,
                }
                .probe(workspace, idle_marker, state, unix_seconds(now), floor);
                let inference = inference.map(|inference| {
                    self.record_idle_inferred(run, workspace, idle_marker, phase, inference)
                });
                self.screen_probes.capture(
                    idle_marker,
                    now,
                    floor,
                    look,
                    inference,
                    base,
                    max_probe_interval(self.stall.screen_idle_secs),
                );
                inference
            }
        };
        Ok(match inference {
            Some(inference) => Some(IdleMarker::inferred(idle_marker, inference)),
            None => marker,
        })
    }

    /// Record `idle_inferred` for the session of `run` once per span, with
    /// the line of its agent's debug log that says its idle hook failed, if
    /// there is one; the inference noted recorded. An event the queue does
    /// not take is recorded on a later capture.
    fn record_idle_inferred(
        &self,
        run: &TaskRun,
        workspace: &str,
        idle_marker: &Path,
        phase: &str,
        inference: Inference,
    ) -> Inference {
        if !inference.unrecorded {
            return inference;
        }
        let mut payload = json!({
            "phase": phase,
            "workspace_id": workspace,
            "source": inference.source,
            "marker": inference.marker.as_str(),
            "since": inference.since,
            "observed_secs": inference.observed_secs,
            "captures": inference.captures,
            "background_running": inference.background_running,
        });
        // A resumed session writes its own debug log.
        let logs = [
            run.run_dir()
                .map(|dir| Path::new(dir).join(RESUME_DEBUG_LOG)),
            run.log_path().map(PathBuf::from),
        ];
        if let Some(line) = logs
            .into_iter()
            .flatten()
            .find_map(|log| screen_idle::hook_failure(&*self.files, self.signals, &log))
        {
            payload["hook_error"] = json!(line);
        }
        if let Err(error) =
            self.queue
                .record_runtime_event(run.id(), EventKind::IdleInferred, payload)
        {
            warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "run {}: idle_inferred could not be recorded: {error:#}", run.id());
            return inference;
        }
        info!(run_id = %run.id(), "session of {} has no fresh idle marker ({}); its screen looks idle since {}", run.id(), inference.marker.as_str(), inference.since);
        self.screen_spans.mark_recorded(&*self.files, idle_marker);
        Inference {
            unrecorded: false,
            ..inference
        }
    }
}

/// The input marker next to the idle marker `idle_marker`: the file the
/// agent's hook replaces each time the session takes an input
/// ([`PROMPT_SUBMIT_MARKER`](crate::application::stats::PROMPT_SUBMIT_MARKER)).
pub(super) fn input_marker_path(idle_marker: &Path) -> PathBuf {
    idle_marker.with_file_name(crate::application::stats::PROMPT_SUBMIT_MARKER)
}

/// An input the session took, as its input marker records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InputMarker {
    /// When the agent took it.
    pub(super) modified: SystemTime,
    pub(super) source: InputSource,
    /// The fingerprint of its text ([`text_fingerprint`]), when the
    /// marker says it.
    pub(super) text: Option<u64>,
}

/// A fingerprint of a text typed into a session, to match the input marker
/// with a text the supervisor typed: its surrounding whitespace ignored.
pub(super) fn text_fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.trim().hash(&mut hasher);
    hasher.finish()
}

impl InputMarker {
    /// The input marker next to `idle_marker`; `None` when the agent's hook
    /// never wrote one (a provider without it, or no input taken yet).
    pub(super) fn read(
        files: &dyn RunFiles,
        signals: &dyn AgentSignals,
        idle_marker: &Path,
    ) -> Result<Option<Self>> {
        let Some((modified, bytes)) = files.read_stamped(&input_marker_path(idle_marker))? else {
            return Ok(None);
        };
        Ok(Some(Self {
            modified,
            source: signals.input_source(&bytes),
            text: signals.input_text(&bytes).as_deref().map(text_fingerprint),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    /// An agent whose marker is `running` while background work runs and
    /// anything else once it is idle.
    struct Signals;

    impl AgentSignals for Signals {
        fn detect_prompt(&self, _: &str) -> Option<&'static str> {
            None
        }

        fn screen_excerpt(&self, screen: &str) -> String {
            screen.to_owned()
        }

        fn idle_hook(&self, content: &[u8]) -> IdleHook {
            IdleHook {
                background_running: content == b"running",
                background_tasks: Vec::new(),
                evidence: vec![("hook_event_name", json!("Stop"))],
            }
        }

        fn input_ready(&self, _: &str) -> bool {
            true
        }

        fn input_pending(&self, _: &str, _: &str) -> bool {
            false
        }

        fn working(&self, _: &str) -> bool {
            false
        }
    }

    fn idle_after_receipt(files: &MemoryFiles, receipt: &Path, marker: &Path) -> Option<Value> {
        IdleMarker::read(files, &Signals, marker)
            .unwrap()
            .and_then(|idle| idle.idle_after_receipt(files, receipt).unwrap())
    }

    #[test]
    fn the_transcript_is_the_whole_screen_by_default() {
        assert_eq!(Signals.transcript("work\n❯\n12:04"), "work\n❯\n12:04");
    }

    #[test]
    fn an_input_after_the_marker_leaves_its_turn_open() {
        let files = MemoryFiles::default();
        let idle = Path::new("/run/idle.json");
        files.put(idle, UNIX_EPOCH + Duration::from_secs(100), "{}");
        let marker = IdleMarker::read(&files, &Signals, idle).unwrap().unwrap();
        let input = |secs| InputMarker {
            modified: UNIX_EPOCH + Duration::from_secs(secs),
            source: InputSource::Agent,
            text: None,
        };
        assert!(!marker.turn_open_after(None));
        assert!(!marker.turn_open_after(Some(&input(100))));
        assert!(!marker.turn_open_after(Some(&input(99))));
        assert!(marker.turn_open_after(Some(&input(101))));
    }

    #[test]
    fn the_input_marker_is_read_next_to_the_idle_marker() {
        let files = MemoryFiles::default();
        let idle = Path::new("/run/idle.json");
        assert_eq!(InputMarker::read(&files, &Signals, idle).unwrap(), None);
        let at = files.now();
        files.put(Path::new("/run/prompt-submit.json"), at, "{}");
        // An adapter that does not read it knows no source.
        assert_eq!(
            InputMarker::read(&files, &Signals, idle).unwrap(),
            Some(InputMarker {
                modified: at,
                source: InputSource::Unknown,
                text: None,
            })
        );
    }

    #[test]
    fn idle_marker_is_idle_unless_background_work_runs() {
        let files = MemoryFiles::default();
        let marker = Path::new("/run/idle.json");
        let receipt = Path::new("/run/receipt.json");
        assert!(
            IdleMarker::read(&files, &Signals, marker)
                .unwrap()
                .is_none()
        );
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        files.write(receipt, b"{}").unwrap();
        let before = files.now() - Duration::from_secs(60);
        for (content, running) in [("{}", false), ("running", true)] {
            files.write(marker, content.as_bytes()).unwrap();
            let idle = IdleMarker::read(&files, &Signals, marker).unwrap().unwrap();
            assert_eq!(idle.background_running(), running, "{content}");
            assert_eq!(idle.idle_since(before), !running, "{content}");
            assert!(!idle.idle_since(files.now() + Duration::from_secs(60)));
            assert_eq!(
                idle_after_receipt(&files, receipt, marker).is_some(),
                !running,
                "{content}"
            );
            // Past the wait, the stop counts whatever still runs.
            let stopped = idle
                .stopped_after_receipt(&files, receipt)
                .unwrap()
                .unwrap();
            assert_eq!(stopped["background_running"], running);
            assert_eq!(stopped["marker_path"], "/run/idle.json");
            assert_eq!(stopped["hook_event_name"], "Stop");
        }
        // Nor does a marker older than the receipt count.
        files.put(marker, files.now() - Duration::from_secs(3600), "{}");
        assert!(idle_after_receipt(&files, receipt, marker).is_none());
        // A receipt that cannot be read is an error, not idleness.
        let idle = IdleMarker::read(&files, &Signals, marker).unwrap().unwrap();
        assert!(
            idle.stopped_after_receipt(&files, Path::new("/run/none"))
                .is_err()
        );
    }

    #[test]
    fn an_idle_the_screen_showed_with_background_work_waits_as_the_markers_would() {
        let files = MemoryFiles::default();
        let receipt = Path::new("/run/receipt.json");
        files.write(receipt, b"{}").unwrap();
        let since = unix_seconds(files.now()) + 1;
        for (read, running) in [(Some(true), true), (Some(false), false)] {
            let idle = IdleMarker::inferred(
                Path::new("/run/idle.json"),
                Inference {
                    source: "screen",
                    marker: MarkerState::Missing,
                    since,
                    observed_secs: 60,
                    captures: 2,
                    background_running: read,
                    unrecorded: false,
                },
            );
            assert_eq!(idle.background_running(), running);
            assert!(idle.background_tasks().is_empty());
            assert_eq!(idle.background_running_evidence(), json!(running));
            assert_eq!(idle.idle_since(UNIX_EPOCH), !running);
            assert_eq!(
                idle.idle_after_receipt(&files, receipt).unwrap().is_some(),
                !running
            );
            let stopped = idle
                .stopped_after_receipt(&files, receipt)
                .unwrap()
                .unwrap();
            assert_eq!(stopped["background_running"], running);
        }
    }

    #[test]
    fn an_idle_the_screen_showed_counts_from_its_span_without_background_known() {
        let files = MemoryFiles::default();
        let receipt = Path::new("/run/receipt.json");
        files.write(receipt, b"{}").unwrap();
        let since = unix_seconds(files.now()) + 1;
        let idle = IdleMarker::inferred(
            Path::new("/run/idle.json"),
            Inference {
                source: "screen",
                marker: MarkerState::Stale,
                since,
                observed_secs: 130,
                captures: 3,
                background_running: None,
                unrecorded: false,
            },
        );
        assert_eq!(unix_seconds(idle.modified()), since);
        assert!(!idle.background_running());
        assert_eq!(idle.background_running_evidence(), Value::Null);
        let evidence = idle.idle_after_receipt(&files, receipt).unwrap().unwrap();
        assert_eq!(evidence["source"], "screen");
        assert_eq!(evidence["marker"], "stale");
        assert_eq!(evidence["observed_secs"], 130);
        assert_eq!(evidence["captures"], 3);
        assert_eq!(evidence["background_running"], Value::Null);
        assert_eq!(evidence["marker_modified"], since);
    }

    #[test]
    fn the_screen_is_captured_again_after_half_the_threshold_and_a_minute_at_most() {
        assert_eq!(probe_interval(120), Duration::from_secs(60));
        assert_eq!(probe_interval(600), Duration::from_secs(60));
        assert_eq!(probe_interval(10), Duration::from_secs(5));
        assert_eq!(probe_interval(1), Duration::ZERO);
    }

    const IDLE_LOOK: ScreenLook = ScreenLook::Idle {
        transcript: 1,
        background: Some(false),
    };

    fn at(secs: u64) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Task 845: capture after capture at work since the same input, the
    /// wait doubles from the probe interval up to four of them; a new
    /// input, or any other look, brings it back.
    #[test]
    fn the_wait_between_captures_grows_while_the_screen_shows_work() {
        let base = probe_interval(120);
        let limit = max_probe_interval(120);
        assert_eq!(limit, Duration::from_secs(240));
        assert_eq!(max_probe_interval(1), Duration::ZERO);
        let marker = Path::new("/run/idle.json");
        let probes = ScreenProbes::default();
        let capture = |now: u64, floor: i64, look: ScreenLook| {
            probes
                .capture(marker, at(now), floor, look, None, base, limit)
                .as_secs()
        };
        let waits: Vec<u64> = (0..5)
            .map(|n| capture(n * 100, 10, ScreenLook::Working))
            .collect();
        assert_eq!(waits, [60, 120, 240, 240, 240]);
        // Not captured again before the wait is out.
        assert_eq!(probes.kept(marker, at(639), 10, base), Some(None));
        assert_eq!(probes.kept(marker, at(640), 10, base), None);
        // An input after the capture brings the wait back at once.
        assert_eq!(probes.kept(marker, at(461), 20, base), None);
        assert_eq!(probes.kept(marker, at(459), 20, base), Some(None));
        // ... and starts the growth over.
        assert_eq!(capture(500, 20, ScreenLook::Working), 60);
        assert_eq!(capture(560, 20, ScreenLook::Working), 120);
        // Any look but work brings it back: at rest, a dialog or no
        // input box, a screen that cannot be read.
        for other in [IDLE_LOOK, ScreenLook::Busy, ScreenLook::Unreadable] {
            assert!(capture(700, 20, ScreenLook::Working) > 60);
            assert_eq!(capture(1000, 20, other), 60, "{other:?}");
            assert_eq!(capture(1100, 20, ScreenLook::Working), 60, "{other:?}");
        }
    }

    /// A session at work for an hour is captured a quarter as often as
    /// every probe interval, and once it comes to rest, its screen is
    /// captured within the longest wait and then every probe interval.
    #[test]
    fn a_session_at_work_is_captured_less_and_its_rest_within_the_limit() {
        let (base, limit) = (probe_interval(120), max_probe_interval(120));
        let marker = Path::new("/run/idle.json");
        let probes = ScreenProbes::default();
        let rest = 3600;
        let mut captures = Vec::new();
        // The supervisor looks every few seconds.
        for now in (0..rest + 600).step_by(5) {
            if probes.kept(marker, at(now), 0, base).is_some() {
                continue;
            }
            let look = if now < rest {
                ScreenLook::Working
            } else {
                IDLE_LOOK
            };
            probes.capture(marker, at(now), 0, look, None, base, limit);
            captures.push(now);
        }
        let working = captures.iter().filter(|&&now| now < rest).count();
        assert!(working <= 18, "{working}: {captures:?}");
        let first_rest = captures.iter().find(|&&now| now >= rest).unwrap();
        assert!(*first_rest < rest + limit.as_secs(), "{captures:?}");
        let after: Vec<u64> = captures
            .windows(2)
            .filter(|pair| pair[0] >= rest)
            .map(|pair| pair[1] - pair[0])
            .collect();
        assert!(
            after.iter().all(|&wait| wait == base.as_secs()),
            "{after:?}"
        );
    }
}
