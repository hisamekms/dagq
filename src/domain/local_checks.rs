//! The records of a worker's local checks (ADR-t2589-1): the project's
//! scripts write one record per run of a check to the directory the
//! runtime names in each of the worker's turns ([`DIR_ENV`]), and the
//! review's material lists the records whose tree is the receipt's
//! commit's. The runtime knows no check's command: what is recorded and
//! with which kind is the project's. A record is the worker's output and
//! may be false (advisory): it is matched to the tree, never trusted, run
//! or interpreted, and neither its presence nor a broken one changes the
//! receipt's acceptance or the run's way.

use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The variable that names the records' directory to a worker's turn
/// (the first, a revise, an answer, a resume): the path the worker sees,
/// inside the run directory a container mounts at its own path. Never
/// given to a command the runtime runs (`integrate`'s verification, the
/// landing recheck, the e2e), whose results the runtime records itself.
pub const DIR_ENV: &str = "DAGQ_LOCAL_CHECKS_DIR";

/// The records' directory, under the run directory.
const DIR: &str = "local-checks";

/// A record is a file of the directory whose name ends with this; the
/// other files (the logs, a script's temporary files) are not records.
pub const RECORD_SUFFIX: &str = ".json";

/// The most bytes a record holds: a larger one is broken.
pub const RECORD_BYTES: u64 = 16 * 1024;

/// The most records read of one run, by name: those after are broken.
pub const MAX_RECORDS: usize = 200;

/// The most characters of a record's command.
const COMMAND_CHARS: usize = 4096;

/// The most characters of a record's kind and its start time.
const SHORT_CHARS: usize = 64;

/// The most bytes of a log's file name.
const LOG_NAME_BYTES: usize = 255;

/// The records' directory of the run directory `run_dir`.
pub fn dir_of(run_dir: &Path) -> PathBuf {
    run_dir.join(DIR)
}

/// Which entries of the records' directory are read as records: the
/// files named `*.json` (by their bytes, so a name that is not UTF-8 is a
/// record too), in the order of their names, up to [`MAX_RECORDS`]; and
/// how many more there are, each a broken record left unread.
pub fn records_to_read(entries: Vec<PathBuf>) -> (Vec<PathBuf>, usize) {
    let mut records = record_paths(entries);
    let over = records.len().saturating_sub(MAX_RECORDS);
    records.truncate(MAX_RECORDS);
    (records, over)
}

/// Which entries of the records' directory the handoff of a new session
/// reads (ADR-t2852-1): the records [`records_to_read`] reads, but the
/// last [`MAX_RECORDS`] by name, so the newest are kept (a project's
/// script names a record by its start, as `scripts/local-check-record.sh`
/// does); and how many older ones are left out unread, which are not
/// broken.
pub fn newest_records_to_read(entries: Vec<PathBuf>) -> (Vec<PathBuf>, usize) {
    let mut records = record_paths(entries);
    let over = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..over);
    (records, over)
}

/// The files of `entries` named `*.json`, in the order of their names.
fn record_paths(entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let suffix = RECORD_SUFFIX.as_bytes();
    let mut records: Vec<_> = entries
        .into_iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.as_encoded_bytes();
                name.len() > suffix.len() && name.ends_with(suffix)
            })
        })
        .collect();
    records.sort();
    records
}

/// One run of a check as its record says, its tree the same at its start
/// and its end. A record is a JSON object of `command` (the command line,
/// a string), `exit_status` (an integer), `started_at` (when it started,
/// a string such as RFC 3339), `duration_ms` (a whole number), `kind`
/// (the project's name of the check, `null` or absent without one),
/// `tree_start` and `tree_end` (the hash of the tree object of the work
/// tree with every change not ignored, as committing it would make, at
/// the start and the end) and `log` (the file name of its whole output in
/// the same directory); other keys are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalCheck {
    pub command: String,
    pub exit_status: i64,
    pub started_at: String,
    pub duration_ms: u64,
    pub kind: Option<String>,
    pub log: String,
    /// The tree it ran on, in lower case.
    #[serde(skip)]
    pub tree: String,
}

/// What one record is to a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// Its tree is the receipt's commit's: a ground of the review.
    Matched(LocalCheck),
    /// It ran on another tree, or the tree changed while it ran.
    OtherTree,
    /// It could not be read, is not a record's shape or is over a limit.
    Broken,
}

/// Read the record `bytes` against `tree`, the receipt's commit's tree;
/// `None` is a record that could not be read or was over
/// [`RECORD_BYTES`].
pub fn judge(bytes: Option<&[u8]>, tree: &str) -> Reading {
    let Some(check) = bytes.and_then(|bytes| parse(bytes).ok()) else {
        return Reading::Broken;
    };
    match check {
        Parsed::Changed => Reading::OtherTree,
        Parsed::Ran(check) if check.tree.eq_ignore_ascii_case(tree) => Reading::Matched(check),
        Parsed::Ran(_) => Reading::OtherTree,
    }
}

enum Parsed {
    Ran(LocalCheck),
    /// Its tree at the end is not its tree at the start.
    Changed,
}

fn parse(bytes: &[u8]) -> Result<Parsed, String> {
    if bytes.len() as u64 > RECORD_BYTES {
        return Err("over the limit".into());
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let object = value.as_object().ok_or("not an object")?;
    let text = |key: &str, limit: usize| -> Result<String, String> {
        let text = object
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("no {key}"))?;
        if text.is_empty() || text.chars().count() > limit {
            return Err(format!("{key} is empty or too long"));
        }
        Ok(text.to_owned())
    };
    let command = text("command", COMMAND_CHARS)?;
    let started_at = text("started_at", SHORT_CHARS)?;
    if started_at.chars().any(char::is_control) {
        return Err("started_at has a control character".into());
    }
    let exit_status = object
        .get("exit_status")
        .and_then(Value::as_i64)
        .ok_or("no exit_status")?;
    let duration_ms = object
        .get("duration_ms")
        .and_then(Value::as_u64)
        .ok_or("no duration_ms")?;
    let kind = match object.get("kind") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let kind = text("kind", SHORT_CHARS)?;
            if !kind
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err("kind is not a name".into());
            }
            Some(kind)
        }
    };
    let log = text("log", LOG_NAME_BYTES)?;
    if log.len() > LOG_NAME_BYTES
        || log == "."
        || log == ".."
        || log.contains('/')
        || log.chars().any(char::is_control)
    {
        return Err("log is not a file name".into());
    }
    let tree_start = text("tree_start", SHORT_CHARS)?;
    let tree_end = text("tree_end", SHORT_CHARS)?;
    if !tree_hash(&tree_start) || !tree_hash(&tree_end) {
        return Err("a tree is not an object hash".into());
    }
    if !tree_start.eq_ignore_ascii_case(&tree_end) {
        return Ok(Parsed::Changed);
    }
    Ok(Parsed::Ran(LocalCheck {
        command,
        exit_status,
        started_at,
        duration_ms,
        kind,
        log,
        tree: tree_start.to_ascii_lowercase(),
    }))
}

/// Whether `text` is a Git object's hash (SHA-1 or SHA-256).
fn tree_hash(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// The records of one run as a review reads them: those on the receipt's
/// commit's tree in the order of their files' names, and how many were
/// left out for another tree or as broken.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalChecks {
    pub matched: Vec<LocalCheck>,
    pub other_tree: usize,
    pub broken: usize,
}

impl LocalChecks {
    pub fn add(&mut self, reading: Reading) {
        match reading {
            Reading::Matched(check) => self.matched.push(check),
            Reading::OtherTree => self.other_tree += 1,
            Reading::Broken => self.broken += 1,
        }
    }

    /// No record at all, matched or not.
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty() && self.other_tree == 0 && self.broken == 0
    }

    /// The matched records' items as the review's material shows them.
    pub fn matched_value(&self) -> Value {
        serde_json::to_value(&self.matched).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const OTHER: &str = "1111111111111111111111111111111111111111";

    fn record(start: &str, end: &str) -> Value {
        json!({
            "command": "make check",
            "exit_status": 0,
            "started_at": "2026-10-17T01:02:03Z",
            "duration_ms": 1500,
            "kind": "gate",
            "tree_start": start,
            "tree_end": end,
            "log": "1.log",
        })
    }

    fn read(value: &Value) -> Reading {
        judge(Some(value.to_string().as_bytes()), TREE)
    }

    #[test]
    fn a_record_whose_start_and_end_are_the_receipt_tree_is_matched() {
        let Reading::Matched(check) = read(&record(TREE, &TREE.to_ascii_uppercase())) else {
            panic!("not matched");
        };
        assert_eq!(
            check,
            LocalCheck {
                command: "make check".into(),
                exit_status: 0,
                started_at: "2026-10-17T01:02:03Z".into(),
                duration_ms: 1500,
                kind: Some("gate".into()),
                log: "1.log".into(),
                tree: TREE.into(),
            }
        );
    }

    #[test]
    fn a_record_of_another_tree_or_whose_tree_changed_is_not_matched() {
        assert_eq!(read(&record(OTHER, OTHER)), Reading::OtherTree);
        assert_eq!(read(&record(TREE, OTHER)), Reading::OtherTree);
        assert_eq!(read(&record(OTHER, TREE)), Reading::OtherTree);
    }

    /// Past the limit of records, the review reads the first by name and
    /// counts the rest, and the handoff the last and counts the older.
    #[test]
    fn past_the_limit_the_review_reads_the_first_records_and_the_handoff_the_newest() {
        let mut entries: Vec<PathBuf> = (0..=MAX_RECORDS)
            .rev()
            .map(|n| PathBuf::from(format!("/d/{n:04}.json")))
            .collect();
        entries.push(PathBuf::from("/d/0001.log"));
        let (first, over) = records_to_read(entries.clone());
        assert_eq!((first.len(), over), (MAX_RECORDS, 1));
        assert_eq!(first[0], PathBuf::from("/d/0000.json"));
        let (newest, older) = newest_records_to_read(entries);
        assert_eq!((newest.len(), older), (MAX_RECORDS, 1));
        assert_eq!(newest[0], PathBuf::from("/d/0001.json"));
        assert_eq!(
            newest.last().unwrap(),
            &PathBuf::from(format!("/d/{MAX_RECORDS:04}.json"))
        );
        let few = vec![PathBuf::from("/d/b.json"), PathBuf::from("/d/a.json")];
        assert_eq!(
            newest_records_to_read(few),
            (
                vec![PathBuf::from("/d/a.json"), PathBuf::from("/d/b.json")],
                0
            )
        );
    }

    #[test]
    fn a_failed_run_and_one_without_a_kind_are_records_all_the_same() {
        let mut value = record(TREE, TREE);
        value["exit_status"] = json!(101);
        value.as_object_mut().unwrap().remove("kind");
        value["extra"] = json!("left out");
        let Reading::Matched(check) = read(&value) else {
            panic!("not matched");
        };
        assert_eq!((check.exit_status, check.kind), (101, None));
        value["kind"] = Value::Null;
        assert!(matches!(read(&value), Reading::Matched(_)));
    }

    #[test]
    fn an_unreadable_malformed_or_oversized_record_is_broken() {
        assert_eq!(judge(None, TREE), Reading::Broken);
        assert_eq!(judge(Some(b"not json"), TREE), Reading::Broken);
        assert_eq!(judge(Some(b"[]"), TREE), Reading::Broken);
        let broken = |key: &str, value: Value| {
            let mut record = record(TREE, TREE);
            record[key] = value;
            read(&record)
        };
        for (key, value) in [
            ("command", json!("")),
            ("command", json!("x".repeat(COMMAND_CHARS + 1))),
            ("command", json!(1)),
            ("exit_status", json!("0")),
            ("exit_status", json!(0.5)),
            ("duration_ms", json!(-1)),
            ("started_at", json!("a\nb")),
            ("kind", json!("two words")),
            ("kind", json!("")),
            ("log", json!("../review.md")),
            ("log", json!("..")),
            ("log", json!("dir/1.log")),
            ("tree_start", json!("abc")),
            ("tree_end", json!(format!("{}zz", &TREE[..38]))),
        ] {
            assert_eq!(broken(key, value.clone()), Reading::Broken, "{key} {value}");
        }
        let mut missing = record(TREE, TREE);
        missing.as_object_mut().unwrap().remove("log");
        assert_eq!(read(&missing), Reading::Broken);
        let mut large = record(TREE, TREE);
        large["padding"] = json!("x".repeat(RECORD_BYTES as usize));
        assert_eq!(read(&large), Reading::Broken);
    }

    fn names(count: usize) -> Vec<PathBuf> {
        (0..count)
            .rev()
            .map(|index| PathBuf::from(format!("/r/local-checks/{index:04}.json")))
            .collect()
    }

    #[test]
    fn only_json_files_are_records_read_in_the_order_of_their_names() {
        use std::os::unix::ffi::OsStrExt;
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/r/local-checks/\xff.json"));
        let entries = vec![
            PathBuf::from("/r/local-checks/b.json"),
            PathBuf::from("/r/local-checks/a.log"),
            PathBuf::from("/r/local-checks/.json"),
            PathBuf::from("/r/local-checks/a.json.tmp"),
            odd.clone(),
            PathBuf::from("/r/local-checks/a.json"),
        ];
        assert_eq!(
            records_to_read(entries),
            (
                vec![
                    PathBuf::from("/r/local-checks/a.json"),
                    PathBuf::from("/r/local-checks/b.json"),
                    odd,
                ],
                0
            )
        );
    }

    #[test]
    fn records_past_the_limit_are_left_unread_and_counted() {
        let (read, over) = records_to_read(names(MAX_RECORDS));
        assert_eq!((read.len(), over), (MAX_RECORDS, 0));
        let (read, over) = records_to_read(names(MAX_RECORDS + 1));
        assert_eq!((read.len(), over), (MAX_RECORDS, 1));
        // The first by name are read, the last left out.
        assert_eq!(read.first(), names(MAX_RECORDS + 1).last());
        assert_eq!(
            read.last().unwrap(),
            &PathBuf::from(format!("/r/local-checks/{:04}.json", MAX_RECORDS - 1))
        );
        assert_eq!(records_to_read(names(MAX_RECORDS + 7)).1, 7);
        assert_eq!(records_to_read(Vec::new()), (Vec::new(), 0));
    }

    #[test]
    fn a_sha256_tree_is_a_tree_hash() {
        let tree = "a".repeat(64);
        assert!(matches!(
            judge(Some(record(&tree, &tree).to_string().as_bytes()), &tree),
            Reading::Matched(_)
        ));
    }

    #[test]
    fn the_tally_counts_each_reading_and_shows_only_the_matched_items() {
        let mut checks = LocalChecks::default();
        assert!(checks.is_empty());
        checks.add(read(&record(TREE, TREE)));
        checks.add(Reading::OtherTree);
        checks.add(Reading::Broken);
        checks.add(Reading::Broken);
        assert_eq!(
            (checks.matched.len(), checks.other_tree, checks.broken),
            (1, 1, 2)
        );
        assert!(!checks.is_empty());
        assert_eq!(
            checks.matched_value(),
            json!([{
                "command": "make check",
                "exit_status": 0,
                "started_at": "2026-10-17T01:02:03Z",
                "duration_ms": 1500,
                "kind": "gate",
                "log": "1.log",
            }])
        );
    }
}
