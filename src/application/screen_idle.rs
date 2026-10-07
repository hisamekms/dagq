//! The inputs a session took, next to its idle marker (ADR-0016): the
//! stamp the supervisor leaves each time it hands a session an input its
//! own hook may not record, and the last input, which an idle marker
//! must be newer than to tell the session's turn ended. No screen is read
//! here: the inference of idleness from a cmux capture (ADR-t803-1) has no
//! session left to judge (ADR-t1433-2, ADR-t1433-5).

use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{RunFiles, unix_seconds};

/// The stamp the supervisor leaves next to the idle marker each time it
/// types a text into the session: an input the agent's own hook may have
/// failed to record.
pub const SUPERVISOR_INPUT_FILE: &str = "supervisor-input.json";
/// The debug log of a run's resumed session, next to its idle marker
/// (the first session's is the run's log path).
pub const RESUME_DEBUG_LOG: &str = "claude-resume.log";

/// Why the idle marker could not tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerState {
    /// The hook never wrote one.
    Missing,
    /// Written before the session's last input.
    Stale,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use std::time::Duration;

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
}
