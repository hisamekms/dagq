//! The audit (ADR-t827-4 decision 2): one JSON line per request, refused
//! ones too, in `<audit dir>/<YYYY-MM-DD>.jsonl` (UTC). No token, signature,
//! key, file content, diff, process output, env value, argument or commit
//! message is ever written: [`AuditRecord`] has no field for them.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long the audit files are kept, in days: older ones are removed when
/// the server starts.
pub const RETENTION_DAYS: u64 = 30;

/// One line of the audit. The fields serialize in this order; a field that
/// does not apply is `null`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    /// When the request was read, RFC 3339 in UTC with milliseconds.
    pub ts: String,
    pub request_id: String,
    /// The token's id, from verified claims only (never from a token that
    /// did not verify).
    pub jti: Option<String>,
    pub run_id: Option<String>,
    pub task_id: Option<u64>,
    pub actor_id: Option<String>,
    /// `fs`, `process` or `git`; `null` for health and unknown routes.
    pub backend: Option<String>,
    /// The operation (`fs.read` and so on); `null` for an unknown route.
    pub op: Option<String>,
    /// The capability the operation needs.
    pub capability: Option<String>,
    /// The resource, relative to the workspace.
    pub path: Option<String>,
    /// `argv[0]`'s basename, for `process.exec`.
    pub program: Option<String>,
    pub argc: Option<usize>,
    /// SHA-256 of the arguments (for matching; not a secret, and guessable
    /// arguments can be recovered from it).
    pub argv_sha256: Option<String>,
    /// The decision: `ok` or the error code.
    pub result: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// Appends records to the day's file. One writer per server; writes are
/// serialized so lines never interleave.
#[derive(Debug)]
pub struct AuditLog {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    /// The audit in `dir`, created if missing.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lock: Mutex::new(()),
        })
    }

    /// The file for the day of `at`.
    pub fn file_for(&self, at: SystemTime) -> PathBuf {
        self.dir.join(format!("{}.jsonl", utc_date(at)))
    }

    /// Append `record` as one line, to the file of the day of `at`.
    pub fn append(&self, at: SystemTime, record: &AuditRecord) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file_for(at))?;
        file.write_all(&line)
    }

    /// Whether a line for the day of `at` could be appended now: the day's
    /// file opens for appending (and is created). The server asks before an
    /// operation runs, so an operation is not done when its line cannot be
    /// written.
    pub fn check(&self, at: SystemTime) -> io::Result<()> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file_for(at))
            .map(drop)
    }

    /// Remove the day files older than [`RETENTION_DAYS`] before `now`, and
    /// return how many were removed. Files that are not day files are left.
    pub fn prune(&self, now: SystemTime) -> io::Result<usize> {
        let cutoff = utc_date(now - Duration::from_secs(RETENTION_DAYS * 86_400));
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let Some(date) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
                continue;
            };
            if is_date(date) && date < cutoff.as_str() {
                fs::remove_file(self.dir.join(&name))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// `YYYY-MM-DD` of `at` in UTC.
pub fn utc_date(at: SystemTime) -> String {
    let (year, month, day) = civil(days_since_epoch(at));
    format!("{year:04}-{month:02}-{day:02}")
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` of `at`.
pub fn rfc3339(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        utc_date(at),
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        since.subsec_millis()
    )
}

fn days_since_epoch(at: SystemTime) -> i64 {
    (at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400) as i64
}

/// The civil date of a day count from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn dates_are_utc_civil_dates() {
        assert_eq!(utc_date(at(0)), "1970-01-01");
        assert_eq!(utc_date(at(951_782_400)), "2000-02-29");
        assert_eq!(utc_date(at(1_790_553_599)), "2026-09-27");
        assert_eq!(utc_date(at(1_790_553_600)), "2026-09-28");
        assert_eq!(
            rfc3339(at(1_790_553_600 + 3723) + Duration::from_millis(45)),
            "2026-09-28T01:02:03.045Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn a_record_is_one_line_with_its_fields_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(&dir.path().join("audit")).unwrap();
        let record = AuditRecord {
            ts: "t".to_owned(),
            request_id: "r".to_owned(),
            result: "unauthorized".to_owned(),
            ..AuditRecord::default()
        };
        log.append(at(1_790_553_600), &record).unwrap();
        log.append(at(1_790_553_601), &record).unwrap();
        let text = fs::read_to_string(dir.path().join("audit/2026-09-28.jsonl")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            r#"{"ts":"t","request_id":"r","jti":null,"run_id":null,"task_id":null,"actor_id":null,"backend":null,"op":null,"capability":null,"path":null,"program":null,"argc":null,"argv_sha256":null,"result":"unauthorized","exit_code":null,"duration_ms":0,"bytes_in":0,"bytes_out":0}"#
        );
        assert_eq!(
            serde_json::from_str::<AuditRecord>(lines[1]).unwrap(),
            record
        );
    }

    #[test]
    fn prune_removes_day_files_past_the_retention_only() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::open(dir.path()).unwrap();
        for name in [
            "2026-08-28.jsonl",
            "2026-08-29.jsonl",
            "2026-09-28.jsonl",
            "notes.jsonl",
            "2026-01-01.txt",
            "2026-1-01.jsonl",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(log.prune(at(1_790_553_600)).unwrap(), 1);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2026-01-01.txt",
                "2026-08-29.jsonl",
                "2026-09-28.jsonl",
                "2026-1-01.jsonl",
                "notes.jsonl"
            ]
        );
    }
}
