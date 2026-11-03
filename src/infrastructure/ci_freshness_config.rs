//! `[[ci_watch.freshness]]` of `dagq.toml` (ADR-t3924-1), read line by
//! line with the rest of the file ([`super::run_env::parse_config`]) into
//! [`FreshnessWatch`]es of `[ci_watch]`. One table per workflow:
//! `workflow` (required, the file name or name `gh` knows it by),
//! `max_age_hours` (required, a positive whole number of hours after its
//! latest success's creation that what it makes may no longer be used)
//! and `warn_before_hours` (a whole number of hours less than
//! `max_age_hours`; left out, a quarter of `max_age_hours`, in seconds):
//! how long before that limit the inbox is told and the workflow started.
//! A mistake (an unknown key, a key twice, a required key missing, a blank
//! workflow, an hour count whose seconds a Unix time in milliseconds
//! cannot hold, a lead not under the limit, the same workflow in two tables,
//! the tables without `[ci_watch]`) is an error with its line.
use anyhow::{Context, Result, bail, ensure};

use super::run_env::{parse_positive, parse_string, strip_comment};
use crate::domain::ci_watch::freshness::FreshnessWatch;

/// `[[ci_watch.freshness]]`, one table per workflow.
pub const CI_FRESHNESS_TABLE: &str = "ci_watch.freshness";

const KEYS: [&str; 3] = ["workflow", "max_age_hours", "warn_before_hours"];
const HOUR: u64 = 3600;
/// The most seconds an hour key may come to: an age is measured in Unix
/// milliseconds (`i64`), so a limit past it could never be reached and is
/// refused rather than read as another value.
const MAX_SECS: u64 = i64::MAX.unsigned_abs() / 1000;

/// `hours` in seconds, when it is no more than [`MAX_SECS`].
fn seconds(hours: u64) -> Option<u64> {
    hours.checked_mul(HOUR).filter(|secs| *secs <= MAX_SECS)
}

/// One table as it is read: its keys read, and the values.
#[derive(Debug, Default)]
struct Open {
    line: usize,
    keys: Vec<String>,
    workflow: String,
    max_age_hours: u64,
    warn_before_hours: Option<u64>,
}

/// The tables of one file as they are read.
#[derive(Debug, Default)]
pub struct FreshnessTables {
    read: Vec<(usize, FreshnessWatch)>,
    /// The table being read, finished at the next table or the file's end.
    open: Option<Open>,
}

impl FreshnessTables {
    /// Start the table whose header is `header` (inside the outer `[`
    /// `]`), on line `number`, if it is `[[ci_watch.freshness]]`; `false`
    /// leaves it to the caller. Any other table ends the one being read.
    pub fn header(&mut self, header: &str, number: usize) -> Result<bool> {
        self.close()?;
        if header == CI_FRESHNESS_TABLE {
            bail!(
                "dagq.toml:{number}: [{CI_FRESHNESS_TABLE}] holds one workflow per table; write [[{CI_FRESHNESS_TABLE}]]"
            );
        }
        let Some(name) = header
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        else {
            return Ok(false);
        };
        if name.trim() != CI_FRESHNESS_TABLE {
            return Ok(false);
        }
        self.open = Some(Open {
            line: number,
            ..Open::default()
        });
        Ok(true)
    }

    /// One `KEY = value` line of the current table, on line `number`;
    /// `rest` is what follows `=`.
    pub fn entry(&mut self, number: usize, key: &str, rest: &str) -> Result<()> {
        let at = |what: String| format!("dagq.toml:{number}: {what}");
        let open = self
            .open
            .as_mut()
            .expect("a [[ci_watch.freshness]] is open");
        ensure!(
            KEYS.contains(&key),
            at(format!(
                "unknown key {key} in [[{CI_FRESHNESS_TABLE}]]; the keys are {}",
                KEYS.join(", ")
            ))
        );
        ensure!(
            !open.keys.iter().any(|seen| seen == key),
            at(format!("{key} is defined twice"))
        );
        open.keys.push(key.to_owned());
        let value = || at(format!("value of {key}"));
        match key {
            "workflow" => {
                let text = parse_string(rest).with_context(value)?;
                ensure!(!text.trim().is_empty(), at(format!("{key} is blank")));
                open.workflow = text;
            }
            "max_age_hours" => {
                let hours = parse_positive(rest, "number of hours")
                    .with_context(value)?
                    .unsigned_abs();
                ensure!(
                    seconds(hours).is_some(),
                    at(format!("{key} is too large: {hours} hours")),
                );
                open.max_age_hours = hours;
            }
            "warn_before_hours" => {
                let digits = strip_comment(rest);
                let hours: u64 = digits
                    .replace('_', "")
                    .parse()
                    .with_context(|| format!("expected a whole number of hours, not {digits}"))
                    .with_context(value)?;
                ensure!(
                    seconds(hours).is_some(),
                    at(format!("{key} is too large: {hours} hours")),
                );
                open.warn_before_hours = Some(hours);
            }
            _ => unreachable!("a key checked above"),
        }
        Ok(())
    }

    /// End the table being read: it has `workflow` and `max_age_hours`, a
    /// lead under the limit, and a workflow no table before named.
    fn close(&mut self) -> Result<()> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        let line = open.line;
        for required in ["workflow", "max_age_hours"] {
            ensure!(
                open.keys.iter().any(|key| key == required),
                "dagq.toml:{line}: [[{CI_FRESHNESS_TABLE}]] has no {required}"
            );
        }
        // Both keys were checked to fit when read.
        let max_age_secs = seconds(open.max_age_hours).unwrap_or(MAX_SECS);
        let lead_secs = match open.warn_before_hours {
            Some(hours) => {
                ensure!(
                    hours < open.max_age_hours,
                    "dagq.toml:{line}: warn_before_hours of [[{CI_FRESHNESS_TABLE}]] must be less than max_age_hours ({}), not {hours}",
                    open.max_age_hours
                );
                seconds(hours).unwrap_or(MAX_SECS)
            }
            None => max_age_secs / 4,
        };
        if let Some((earlier, _)) = self
            .read
            .iter()
            .find(|(_, watch)| watch.workflow == open.workflow)
        {
            bail!(
                "dagq.toml:{line}: [[{CI_FRESHNESS_TABLE}]] repeats workflow {:?} of line {earlier}",
                open.workflow
            );
        }
        self.read.push((
            line,
            FreshnessWatch {
                workflow: open.workflow,
                max_age_secs,
                lead_secs,
            },
        ));
        Ok(())
    }

    /// The tables read in file order, once the file ended, with the line
    /// of the first (`None` without any).
    pub fn finish(mut self) -> Result<(Option<usize>, Vec<FreshnessWatch>)> {
        self.close()?;
        let first = self.read.first().map(|(line, _)| *line);
        Ok((
            first,
            self.read.into_iter().map(|(_, watch)| watch).collect(),
        ))
    }
}
