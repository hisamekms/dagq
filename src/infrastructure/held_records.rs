//! The records of results the queue service did not take, kept in the
//! queue's service directory ([`HELD_DIR`]), one file each, until a
//! supervisor sends them (ADR-t3102-3 decision 5): those a supervisor still
//! held when it ended or execed, the ends of jobs it could not write, and
//! those a job command gave up sending; and, apart ([`VERDICTS_DIR`]), the
//! verdicts of reviews that ended which the queue did not take. The
//! directories are the control side's, beside the service's tokens.

use crate::application::supervisor_service::resend::{HeldRecords, KeptRecord};
use crate::domain::queue_service::{HELD_DIR, UseCase, VERDICTS_DIR};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The records kept for the queue at a DB, in its service directory.
pub struct HeldDir {
    dir: PathBuf,
    /// The time a record is kept at, in Unix milliseconds, that its file's
    /// name begins with.
    now_ms: fn() -> i64,
}

/// The count of records this process kept, in their files' names.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// What one file holds.
#[derive(Serialize, Deserialize)]
struct Kept {
    use_case: UseCase,
    params: Value,
}

impl HeldDir {
    /// The records kept for the queue at `db`.
    pub fn of(db: &Path) -> Self {
        Self::at(
            super::queue_service::service_dir(&super::queue_service::queue_dir_of(db))
                .join(HELD_DIR),
        )
    }

    /// The review verdicts kept for the queue at `db`, in its service
    /// directory.
    pub fn verdicts_of(db: &Path) -> Self {
        Self::at(
            super::queue_service::service_dir(&super::queue_service::queue_dir_of(db))
                .join(VERDICTS_DIR),
        )
    }

    /// The records kept in `dir`.
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            now_ms: super::queue_service::unix_now_ms,
        }
    }

    /// The same records with their times read from `now_ms`.
    #[must_use]
    pub fn timed_by(self, now_ms: fn() -> i64) -> Self {
        Self { now_ms, ..self }
    }
}

impl HeldRecords for HeldDir {
    /// A file named by when it was kept, the process and its count in it,
    /// so the names sort oldest first.
    fn keep(&self, use_case: UseCase, params: &Value) -> Result<String> {
        super::queue_service::private_dir(&self.dir)?;
        let name = format!(
            "{:013}-{:010}-{:08}.json",
            (self.now_ms)(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let body = serde_json::to_vec(&Kept {
            use_case,
            params: params.clone(),
        })?;
        super::queue_service::write_private(&self.dir.join(&name), &body)?;
        Ok(name)
    }

    /// A file that does not read is let go: it is no record to send.
    fn kept(&self) -> Result<Vec<KeptRecord>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            entries => entries?,
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
            .collect();
        names.sort();
        let mut records = Vec::new();
        for name in names {
            let path = self.dir.join(&name);
            let bytes = match std::fs::read(&path) {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                bytes => bytes?,
            };
            match serde_json::from_slice::<Kept>(&bytes) {
                Ok(kept) => records.push(KeptRecord {
                    key: name,
                    use_case: kept.use_case,
                    params: kept.params,
                }),
                Err(error) => {
                    tracing::warn!(
                        "the kept record {} does not read and is let go: {error}",
                        path.display()
                    );
                    self.forget(&name)?;
                }
            }
        }
        Ok(records)
    }

    fn forget(&self, key: &str) -> Result<()> {
        ensure!(
            !key.contains('/') && !key.starts_with('.'),
            "{key} is no kept record"
        );
        match std::fs::remove_file(self.dir.join(key)) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            removed => Ok(removed?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_are_kept_in_order_and_let_go_once_sent() {
        let dir = tempfile::tempdir().unwrap();
        let held = HeldDir::at(dir.path().join("held")).timed_by(|| 1_790_000_000_000);
        assert!(held.kept().unwrap().is_empty());
        for n in 1..=3 {
            held.keep(
                UseCase::Supervisor,
                &json!({"op": "record_queue_event", "n": n}),
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("held/9999999999999-x.json"), "not json").unwrap();
        std::fs::write(dir.path().join("held/.partial.tmp"), "{}").unwrap();
        let kept = held.kept().unwrap();
        let ns: Vec<&Value> = kept.iter().map(|record| &record.params["n"]).collect();
        assert_eq!(ns, [1, 2, 3]);
        assert!(
            kept.iter()
                .all(|record| record.use_case == UseCase::Supervisor)
        );
        held.forget(&kept[0].key).unwrap();
        held.forget(&kept[0].key).unwrap();
        assert!(held.forget("../outside.json").is_err());
        let left: Vec<String> = held
            .kept()
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        assert_eq!(left, [kept[1].key.clone(), kept[2].key.clone()]);
        // Another process's held records of the same queue are the same.
        assert_eq!(
            HeldDir::at(dir.path().join("held")).kept().unwrap().len(),
            2
        );
    }
}
