//! `[[recurring]]` of `dagq.toml` (ADR-0072 decision 1), read line by line
//! with the rest of the file ([`super::run_env::parse_config`]) into
//! [`RecurringDraft`]s, whose doc comment has what each key means. A
//! mistake (an unknown key, a key twice, a required key missing, a blank
//! string, an interval or goal that is not a positive whole number, a path
//! glob or evidence a task would refuse, the same key in two tables) is an
//! error with its line.
use anyhow::{Context, Result, bail, ensure};

use super::run_env::{parse_positive, parse_string, parse_string_array};
use crate::domain::{EvidenceCheck, GoalId, recurring::RecurringDraft, scope};

/// `[[recurring]]`, one table per proposal.
pub const RECURRING_TABLE: &str = "recurring";

const KEYS: [&str; 9] = [
    "key",
    "interval_days",
    "title",
    "description",
    "acceptance",
    "verification",
    "paths",
    "evidence",
    "goal",
];
/// Every key but `goal` is required.
const REQUIRED: [&str; 8] = [
    "key",
    "interval_days",
    "title",
    "description",
    "acceptance",
    "verification",
    "paths",
    "evidence",
];

/// The tables of one file as they are read.
#[derive(Debug, Default)]
pub struct RecurringTables {
    read: Vec<(usize, RecurringDraft)>,
    /// The line of the table being read and its keys read, finished at the
    /// next table or the file's end.
    open: Option<(usize, Vec<String>, RecurringDraft)>,
}

impl RecurringTables {
    /// Start the table whose header is `header` (inside the outer `[`
    /// `]`), on line `number`, if it is `[[recurring]]`; `false` leaves it
    /// to the caller. Any other table ends the one being read.
    pub fn header(&mut self, header: &str, number: usize) -> Result<bool> {
        self.close()?;
        if header == RECURRING_TABLE {
            bail!(
                "dagq.toml:{number}: [{RECURRING_TABLE}] holds one proposal per table; write [[{RECURRING_TABLE}]]"
            );
        }
        let Some(name) = header
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
        else {
            return Ok(false);
        };
        if name.trim() != RECURRING_TABLE {
            return Ok(false);
        }
        self.open = Some((
            number,
            Vec::new(),
            RecurringDraft {
                key: String::new(),
                interval_days: 0,
                title: String::new(),
                description: String::new(),
                acceptance: String::new(),
                verification: Vec::new(),
                paths: Vec::new(),
                evidence: Vec::new(),
                goal: None,
            },
        ));
        Ok(true)
    }

    /// One `KEY = value` line of the current table, on line `number`;
    /// `rest` is what follows `=`.
    pub fn entry(&mut self, number: usize, key: &str, rest: &str) -> Result<()> {
        let at = |what: String| format!("dagq.toml:{number}: {what}");
        let (_, keys, draft) = self.open.as_mut().expect("a [[recurring]] is open");
        ensure!(
            KEYS.contains(&key),
            at(format!(
                "unknown key {key} in [[{RECURRING_TABLE}]]; the keys are {}",
                KEYS.join(", ")
            ))
        );
        ensure!(
            !keys.iter().any(|seen| seen == key),
            at(format!("{key} is defined twice"))
        );
        keys.push(key.to_owned());
        let value = || at(format!("value of {key}"));
        let text = || -> Result<String> {
            let text = parse_string(rest).with_context(value)?;
            ensure!(!text.trim().is_empty(), at(format!("{key} is blank")));
            Ok(text)
        };
        let list = || -> Result<Vec<String>> {
            let items = parse_string_array(rest).with_context(value)?;
            ensure!(
                items.iter().all(|item| !item.trim().is_empty()),
                at(format!("{key} has a blank item"))
            );
            Ok(items)
        };
        match key {
            "key" => draft.key = text()?,
            "interval_days" => {
                draft.interval_days = parse_positive(rest, "number of days").with_context(value)?;
            }
            "title" => draft.title = text()?,
            "description" => draft.description = text()?,
            "acceptance" => draft.acceptance = text()?,
            "verification" => draft.verification = list()?,
            "paths" => {
                let paths = list()?;
                scope::validate_path_globs(&paths)
                    .map_err(|error| anyhow::anyhow!(at(format!("{key}: {error}"))))?;
                draft.paths = paths;
            }
            "evidence" => {
                let mut checks = Vec::new();
                for item in list()? {
                    let check: EvidenceCheck = item.parse().map_err(|_| {
                        anyhow::anyhow!(at(format!(
                            "{key} names {item:?}; the checks are tests, e2e and subagent_review"
                        )))
                    })?;
                    ensure!(
                        !checks.contains(&check),
                        at(format!("{key} names {item:?} twice"))
                    );
                    checks.push(check);
                }
                draft.evidence = checks;
            }
            "goal" => {
                draft.goal = Some(GoalId::new(
                    parse_positive(rest, "goal ID").with_context(value)?,
                ));
            }
            _ => unreachable!("a key checked above"),
        }
        Ok(())
    }

    /// End the table being read: it has every required key, and its key
    /// is not one read before.
    fn close(&mut self) -> Result<()> {
        let Some((line, keys, draft)) = self.open.take() else {
            return Ok(());
        };
        for required in REQUIRED {
            ensure!(
                keys.iter().any(|key| key == required),
                "dagq.toml:{line}: [[{RECURRING_TABLE}]] has no {required}"
            );
        }
        if let Some((earlier, _)) = self.read.iter().find(|(_, d)| d.key == draft.key) {
            bail!(
                "dagq.toml:{line}: [[{RECURRING_TABLE}]] repeats key {:?} of line {earlier}",
                draft.key
            );
        }
        self.read.push((line, draft));
        Ok(())
    }

    /// The tables read in file order, once the file ended.
    pub fn finish(mut self) -> Result<Vec<RecurringDraft>> {
        self.close()?;
        Ok(self.read.into_iter().map(|(_, draft)| draft).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::run_env::parse_config;
    use super::*;

    const RUST: &str = "[[recurring]]
key = \"rust-toolchain\"
interval_days = 42 # six weeks
title = 'chore: raise Rust to the latest stable'
description = \"Check the latest stable.\"
acceptance = \"fmt, clippy and test pass\"
verification = [\"cargo fmt --all --check\", \"cargo test\"]
paths = [\"rust-toolchain.toml\", \"Cargo.toml\"]
evidence = [\"tests\"]
";

    /// Each table is one proposal in file order, with every key, `goal`
    /// optional, and the other tables of the file read around it.
    #[test]
    fn parses_the_recurring_tables() {
        let text = format!(
            "{RUST}goal = 46\n\n[[recurring]]\nkey = 'audit'\ninterval_days = 7\ntitle = 't'\ndescription = 'd'\nacceptance = 'a'\nverification = []\npaths = []\nevidence = []\n\n[e2e]\npaths = [\"tests/e2e/**\"]\n"
        );
        let config = parse_config(&text).unwrap();
        assert_eq!(config.recurring.len(), 2);
        let rust = &config.recurring[0];
        assert_eq!(rust.key, "rust-toolchain");
        assert_eq!(rust.interval_days, 42);
        assert_eq!(rust.title, "chore: raise Rust to the latest stable");
        assert_eq!(rust.verification.len(), 2);
        assert_eq!(rust.paths, vec!["rust-toolchain.toml", "Cargo.toml"]);
        assert_eq!(rust.evidence, vec![EvidenceCheck::Tests]);
        assert_eq!(rust.goal, Some(GoalId::new(46)));
        assert_eq!(config.recurring[1].goal, None);
        assert_eq!(config.e2e_paths, vec!["tests/e2e/**"]);
        assert!(parse_config("").unwrap().recurring.is_empty());
    }

    /// Each mistake of the format is an error with its line.
    #[test]
    fn refuses_a_mistaken_recurring_table() {
        let without = |key: &str| {
            RUST.lines()
                .filter(|line| !line.starts_with(&format!("{key} =")))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let cases = [
            (
                "[recurring]\nkey = 'k'\n".to_owned(),
                "dagq.toml:1: [recurring] holds one proposal per table",
            ),
            (without("paths"), "dagq.toml:1: [[recurring]] has no paths"),
            (
                without("evidence"),
                "dagq.toml:1: [[recurring]] has no evidence",
            ),
            (
                format!("{RUST}color = 'red'\n"),
                "dagq.toml:10: unknown key color in [[recurring]]",
            ),
            (
                format!("{RUST}key = 'again'\n"),
                "dagq.toml:10: key is defined twice",
            ),
            (
                RUST.replace("= 42 # six weeks", "= 0"),
                "dagq.toml:3: value of interval_days",
            ),
            (
                RUST.replace("= 42 # six weeks", "= '42'"),
                "dagq.toml:3: value of interval_days",
            ),
            (
                RUST.replace(
                    "title = 'chore: raise Rust to the latest stable'",
                    "title = ' '",
                ),
                "dagq.toml:4: title is blank",
            ),
            (
                RUST.replace("[\"tests\"]", "[\"lint\"]"),
                "dagq.toml:9: evidence names \"lint\"",
            ),
            (
                RUST.replace("[\"tests\"]", "[\"tests\", \"tests\"]"),
                "dagq.toml:9: evidence names \"tests\" twice",
            ),
            (
                RUST.replace("\"Cargo.toml\"]", "\"../x\"]"),
                "dagq.toml:8: paths:",
            ),
            (format!("{RUST}goal = -1\n"), "dagq.toml:10: value of goal"),
            (
                format!("{RUST}\n{RUST}"),
                "dagq.toml:11: [[recurring]] repeats key \"rust-toolchain\" of line 1",
            ),
        ];
        for (text, expected) in cases {
            let error = format!("{:#}", parse_config(&text).unwrap_err());
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }
}
