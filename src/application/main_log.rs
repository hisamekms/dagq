//! What a supervisor's pass records of main's first-parent history
//! (docs/design/main-history.md): the commits since the head
//! it recorded last, its reach at an interval while the head does not move,
//! and the start and end of a failure to read Git. The fold that reads the
//! record back is [`crate::domain::stats::main_log::fold_main_log`]; the
//! runtime decides nothing on the record.

use anyhow::Result;
use serde_json::Value;

use super::RepositoryReads;
use crate::domain::{
    event_kind::EventKind,
    stats::{
        conflicts::MainCommit,
        main_log::{
            MAIN_OBSERVED_INTERVAL_SECS, MainReading, Reach, commits_payloads, observed_payload,
            paths_payloads, read_failed_payload, read_recovered_payload, reverts_imported_payload,
            rewritten_payload,
        },
    },
};

/// What the record reads of Git: a narrowing of [`RepositoryReads`] to
/// its reads of main, implemented for every `RepositoryReads` and by test
/// fakes. It is not a port of its own.
pub trait MainReader {
    /// Main's head now.
    fn head(&self) -> Result<String>;
    /// See [`RepositoryReads::main_commits`].
    fn commits(&self, head: &str, after: Option<&str>, since: i64) -> Result<Vec<MainCommit>>;
    /// See [`RepositoryReads::tree_paths`].
    fn paths(&self, commit: &str) -> Result<Vec<String>>;
    /// Whether `commit` is still on main's first-parent line at `head`.
    fn reaches(&self, commit: &str, head: &str) -> Result<bool>;
    /// Their merge base, `None` without one (or without `a`).
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
    /// Whether Git has `commit`.
    fn has(&self, commit: &str) -> Result<bool>;
}

impl<T: RepositoryReads + ?Sized> MainReader for T {
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
    fn has(&self, commit: &str) -> Result<bool> {
        self.has_commit(commit)
    }
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        if !self.has_commit(a)? {
            return Ok(None);
        }
        Ok(RepositoryReads::merge_base(self, a, b)?.map(|sha| sha.as_str().to_owned()))
    }
}

/// What the queue holds of the record when a pass starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recorded {
    /// The latest `main_observed`; `None` before the first record.
    pub reach: Option<Reach>,
    /// The latest change of whether Git can be read.
    pub reading: MainReading,
    /// The heads of the lines recorded before the record carried the
    /// evidence of a revert besides the reach's
    /// ([`crate::domain::stats::main_log::import_heads`]): read only while
    /// no reach carries the evidence ([`Reach::reverts`], none before the
    /// first reach), for the import of every line recorded.
    pub lines: Vec<String>,
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
/// once [`MAIN_OBSERVED_INTERVAL_SECS`] passed. A reach recorded before the
/// commits carried the evidence of a revert ([`Reach::reverts`]) has the
/// evidence of every line recorded (the reach's head and
/// [`Recorded::lines`]) from the record's start imported first, and so
/// does a first record after commits a pass recorded without the evidence
/// and left before its reach
/// (`main_reverts_imported`) and the reach recorded after, whether due or
/// not: once that reach is written no pass imports again, and a pass that
/// stopped before it imports again, which the fold counts once. A line
/// whose head Git no longer has is named `unread` and the reach is written
/// all the same: Git will not have it later either, and the readers keep
/// its reverts unknown ([`crate::domain::ledger::post_landing`]). Git
/// failed: the start of the failure, unless one is in place, and nothing
/// else.
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
    // The range the readers of Git read, for the first record: from a
    // second before the queue's earliest event.
    let first_since =
        || -> Result<i64> { Ok(earliest()?.map_or(now - 1, |millis| millis.div_euclid(1000) - 1)) };
    // An import of the lines recorded without the evidence: after a reach
    // without it, or before any reach after a record a pass left before it.
    let import = match &recorded.reach {
        Some(reach) if !reach.reverts => Some((Some(reach.head.as_str()), reach.since)),
        None if !recorded.lines.is_empty() => Some((None, first_since()?)),
        _ => None,
    };
    if let Some((reach_head, since)) = import {
        // Every line recorded, whose commits the record keeps as main's
        // history: the reach's and the others.
        let mut heads: Vec<&str> = Vec::new();
        for tip in reach_head
            .into_iter()
            .chain(recorded.lines.iter().map(String::as_str))
        {
            if !heads.contains(&tip) {
                heads.push(tip);
            }
        }
        let (mut read, mut unread, mut commits) = (Vec::new(), Vec::new(), Vec::new());
        for tip in heads {
            if reader.has(tip)? {
                commits.extend(reader.commits(tip, None, since)?);
                read.push(tip.to_owned());
            } else {
                unread.push(tip.to_owned());
            }
        }
        records.push((
            EventKind::MainRevertsImported,
            reverts_imported_payload(&read, &unread, since, now, &commits),
        ));
    }
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
            let since = first_since()?;
            commits(&mut records, None, since)?;
            base(&mut records, since)?;
            since
        }
        Some(reach) if reach.head == head => {
            let failing = matches!(recorded.reading, MainReading::Failing { .. });
            if import.is_none() && !failing && now - reach.at < MAIN_OBSERVED_INTERVAL_SECS {
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
