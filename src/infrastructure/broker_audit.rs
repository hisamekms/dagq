//! The adapter of the broker's audit dir
//! ([`crate::application::broker_admin::AuditFiles`]): [`AuditDir`] lists
//! and reads `<queue dir>/broker/audit` as the broker wrote it.

use std::fs;
use std::io::{ErrorKind, Result};
use std::path::PathBuf;

use crate::application::broker_admin::AuditFiles;

/// The audit dir on the host's file system.
#[derive(Debug, Clone)]
pub struct AuditDir {
    pub dir: PathBuf,
}

impl AuditDir {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl AuditFiles for AuditDir {
    fn names(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names = Vec::new();
        for entry in entries {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        Ok(names)
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::application::broker_admin::{AuditQuery, audit};

    #[test]
    fn the_dir_lists_and_reads_its_files_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = AuditDir::new(dir.path().join("broker/audit"));
        assert!(missing.names().unwrap().is_empty());
        assert_eq!(missing.read("2026-09-28.jsonl").unwrap(), None);

        fs::write(dir.path().join("2026-09-28.jsonl"), "line\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "").unwrap();
        let files = AuditDir::new(dir.path().to_owned());
        let mut names = files.names().unwrap();
        names.sort();
        assert_eq!(names, ["2026-09-28.jsonl", "notes.txt"]);
        assert_eq!(
            files.read("2026-09-28.jsonl").unwrap().as_deref(),
            Some(&b"line\n"[..])
        );
        assert_eq!(files.read("2026-09-27.jsonl").unwrap(), None);
        // A dir where a file is wanted is an error, not a missing file.
        fs::create_dir(dir.path().join("2026-09-29.jsonl")).unwrap();
        assert!(files.read("2026-09-29.jsonl").is_err());
    }

    #[test]
    fn audit_without_its_dir_is_no_line() {
        let dir = tempfile::tempdir().unwrap();
        let files = AuditDir::new(dir.path().join("broker/audit"));
        let report = audit(&files, &AuditQuery::default()).unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"entries": [], "skipped": 0, "dropped": 0})
        );
    }
}
