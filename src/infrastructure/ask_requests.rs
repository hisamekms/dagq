//! The file of a Codex worker's ask request (ADR-t813-3 decision 3): what
//! its `dagq ask` writes to the run directory instead of the queue, which
//! its sandbox does not let it write. The supervisor opens it
//! (`application::supervise::ask_requests`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::domain::ask_request::{AskRequest, pending_name};

/// Write `request` to `dir` as `<id>.json`, through a temporary file of
/// the same directory and a rename, so the supervisor never reads half of
/// it; the path written.
pub fn write(dir: &Path, request: &AskRequest) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(pending_name(&request.id));
    let temporary = dir.join(format!(".{}.tmp", request.id));
    fs::write(&temporary, serde_json::to_vec_pretty(request)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("rename {} to {}", temporary.display(), path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ask_request::pending_id;

    #[test]
    fn a_request_is_written_whole_under_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("ask-requests");
        let request = AskRequest {
            id: "abc".to_owned(),
            kind: "worker_question".to_owned(),
            because: "scope".to_owned(),
            question: "Which?".to_owned(),
            options: vec![],
            topics: vec!["other".to_owned()],
            run_id: Some("r".to_owned()),
            task_id: None,
            finding_id: None,
        };
        let path = write(&dir, &request).unwrap();
        assert_eq!(
            pending_id(path.file_name().unwrap().to_str().unwrap()),
            Some("abc")
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temporary file is left: {names:?}");
        let read: AskRequest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, request);
    }
}
