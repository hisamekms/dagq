//! What a supervisor's pass records of main's first-parent history
//! (docs/design/main-history.md): the commits since the head
//! it recorded last, its reach at an interval while the head does not move,
//! and the start and end of a failure to read Git. The fold that reads the
//! record back is [`crate::domain::stats::main_log::fold_main_log`]; the
//! runtime decides nothing on the record.

use anyhow::Result;
use serde_json::Value;

use super::Repository;
use crate::domain::{
    event_kind::EventKind,
    stats::{
        conflicts::MainCommit,
        main_log::{
            MAIN_OBSERVED_INTERVAL_SECS, MainReading, Reach, commits_payloads, observed_payload,
            paths_payloads, read_failed_payload, read_recovered_payload, rewritten_payload,
        },
    },
};

/// What the record reads of Git: a narrowing of [`Repository`] to its
/// read-only reads of main, implemented for every `Repository` and by test
/// fakes. It is not a port of its own.
pub trait MainReader {
    /// Main's head now.
    fn head(&self) -> Result<String>;
    /// See [`Repository::main_commits`].
    fn commits(&self, head: &str, after: Option<&str>, since: i64) -> Result<Vec<MainCommit>>;
    /// See [`Repository::tree_paths`].
    fn paths(&self, commit: &str) -> Result<Vec<String>>;
    /// Whether `commit` is still on main's first-parent line at `head`.
    fn reaches(&self, commit: &str, head: &str) -> Result<bool>;
    /// Their merge base, `None` without one (or without `a`).
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
}

impl<T: Repository + ?Sized> MainReader for T {
    fn head(&self) -> Result<String> {
        Ok(self.main_head()?.as_str().to_owned())
    }
    fn commits(&self, head: &str, after: Option<&str>, since: i64) -> Result<Vec<MainCommit>> {
        self.main_commits(head, after, since)
    }
    fn paths(&self, commit: &str) -> Result<Vec<String>> {
        self.tree_paths(commit)
    }
    fn reaches(&self, commit: &str, head: &str) -> Result<bool> {
        self.on_first_parent_line(commit, head)
    }
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        if !self.has_commit(a)? {
            return Ok(None);
        }
        Ok(Repository::merge_base(self, a, b)?.map(|sha| sha.as_str().to_owned()))
    }
}

/// What the queue holds of the record when a pass starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recorded {
    /// The latest `main_observed`; `None` before the first record.
    pub reach: Option<Reach>,
    /// The latest change of whether Git can be read.
    pub reading: MainReading,
}

/// The events a pass records, in order.
pub type Records = Vec<(EventKind, Value)>;

/// The events to record at `now` (unix seconds) after `recorded`, reading
/// Git through `reader`; `earliest` gives the unix millisecond of the
/// queue's earliest event, read only for the first record. Git read fine:
/// the end of a failure in place, then the commits since the reach (all of
/// them since a second before the earliest event, with the base paths, the
/// first time; from the merge base with the base paths again, when the
/// reach left main's first-parent line) and the reach, which is recorded alone only
/// once [`MAIN_OBSERVED_INTERVAL_SECS`] passed. Git failed: the start of
/// the failure, unless one is in place, and nothing else.
pub fn records<R: MainReader + ?Sized>(
    recorded: &Recorded,
    now: i64,
    earliest: &dyn Fn() -> Result<Option<i64>>,
    reader: &R,
) -> Records {
    match read(recorded, now, earliest, reader) {
        Ok(read) => {
            let mut records = Vec::new();
            if let MainReading::Failing { since, reason } = &recorded.reading {
                records.push((
                    EventKind::MainReadRecovered,
                    read_recovered_payload(reason, *since, now),
                ));
            }
            records.extend(read);
            records
        }
        Err(error) => match recorded.reading {
            MainReading::Failing { .. } => Vec::new(),
            MainReading::Readable => vec![(
                EventKind::MainReadFailed,
                read_failed_payload(&format!("{error:#}"), now),
            )],
        },
    }
}

fn read<R: MainReader + ?Sized>(
    recorded: &Recorded,
    now: i64,
    earliest: &dyn Fn() -> Result<Option<i64>>,
    reader: &R,
) -> Result<Records> {
    let head = reader.head()?;
    let mut records = Records::new();
    let commits = |records: &mut Records, after: Option<&str>, since| -> Result<()> {
        let commits = reader.commits(&head, after, since)?;
        records.extend(
            commits_payloads(after, &commits)
                .into_iter()
                .map(|payload| (EventKind::MainCommitsRecorded, payload)),
        );
        Ok(())
    };
    let base = |records: &mut Records, since| -> Result<()> {
        let paths = reader.paths(&head)?;
        records.extend(
            paths_payloads(&head, since, &paths)
                .into_iter()
                .map(|payload| (EventKind::MainPathsRecorded, payload)),
        );
        Ok(())
    };
    let since = match &recorded.reach {
        None => {
            // The range the readers of Git read: from a second before the
            // queue's earliest event.
            let since = earliest()?.map_or(now - 1, |millis| millis.div_euclid(1000) - 1);
            commits(&mut records, None, since)?;
            base(&mut records, since)?;
            since
        }
        Some(reach) if reach.head == head => {
            let failing = matches!(recorded.reading, MainReading::Failing { .. });
            if !failing && now - reach.at < MAIN_OBSERVED_INTERVAL_SECS {
                return Ok(records);
            }
            reach.since
        }
        Some(reach) if reader.reaches(&reach.head, &head)? => {
            commits(&mut records, Some(&reach.head), reach.since)?;
            reach.since
        }
        Some(reach) => {
            // A merge base off the head's first-parent line is no point to
            // go on from: the line is recorded again from the start.
            let merge_base = match reader.merge_base(&reach.head, &head)? {
                Some(base) if reader.reaches(&base, &head)? => Some(base),
                _ => None,
            };
            records.push((
                EventKind::MainRewritten,
                rewritten_payload(&reach.head, &head, merge_base.as_deref(), now),
            ));
            commits(&mut records, merge_base.as_deref(), reach.since)?;
            base(&mut records, reach.since)?;
            reach.since
        }
    };
    records.push((EventKind::MainObserved, observed_payload(&head, now, since)));
    Ok(records)
}

#[cfg(test)]
mod tests;
