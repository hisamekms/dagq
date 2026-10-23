//! `[telemetry]` and `[[telemetry.commands]]` of `dagq.toml`, read line by
//! line with the rest of the file ([`super::run_env::parse_config`]) into
//! [`TelemetrySettings`], whose doc comment has their format and the
//! meaning of each key. A mistake (an unknown key or value, a key twice,
//! a rule without its class or program, a class not of a class's form, a
//! `lead` that is no reserved word, the same rule twice) is an error with
//! its line.
use anyhow::{Context, Result, bail, ensure};

use super::run_env::{parse_bool, parse_string, parse_string_array, strip_comment};
use crate::domain::command_rules::{
    CommandDetail, CommandRule, LEAD_WORDS, Positional, RESERVED_CLASSES, TelemetrySettings,
    valid_class,
};

/// `[telemetry]`.
pub const TELEMETRY_TABLE: &str = "telemetry";
/// `[[telemetry.commands]]`, one table per rule.
pub const COMMANDS_TABLE: &str = "telemetry.commands";

const COMMAND_DETAIL: &str = "command_detail";
const FAILED_TESTS: &str = "failed_tests";
const RULE_KEYS: [&str; 11] = [
    "class",
    "group",
    "program",
    "sub",
    "flags",
    "none_of",
    "flag_values",
    "takes_value",
    "positional",
    "args_any",
    "lead",
];

/// The tables of one file as they are read.
#[derive(Debug, Default)]
pub struct TelemetryTables {
    settings: TelemetrySettings,
    /// The line of `[telemetry]`, once read.
    telemetry_seen: Option<usize>,
    /// Which table the lines are in.
    in_table: Option<Table>,
    /// The keys of the current table read.
    keys: Vec<String>,
    /// The rule being read, finished at the next table or the file's end.
    open: Option<CommandRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Telemetry,
    Command,
}

impl TelemetryTables {
    /// Start the table whose header is `header` (inside the outer `[`
    /// `]`), on line `number`, if it is one of these; `false` leaves it to
    /// the caller. Any other table ends the one being read.
    pub fn header(&mut self, header: &str, number: usize) -> Result<bool> {
        self.close()?;
        self.in_table = None;
        if let Some(inner) = header.strip_prefix('[') {
            let name = inner
                .strip_suffix(']')
                .with_context(|| format!("dagq.toml:{number}: unclosed table header [{header}]"))?
                .trim();
            if name != COMMANDS_TABLE {
                return Ok(false);
            }
            self.open = Some(CommandRule {
                line: Some(number),
                ..CommandRule::new("", "")
            });
            self.keys.clear();
            self.in_table = Some(Table::Command);
            return Ok(true);
        }
        match header {
            TELEMETRY_TABLE => {
                ensure!(
                    self.telemetry_seen.is_none(),
                    "dagq.toml:{number}: [{TELEMETRY_TABLE}] is defined twice"
                );
                self.telemetry_seen = Some(number);
                self.keys.clear();
                self.in_table = Some(Table::Telemetry);
                Ok(true)
            }
            COMMANDS_TABLE => bail!(
                "dagq.toml:{number}: [{COMMANDS_TABLE}] holds one rule per table; write [[{COMMANDS_TABLE}]]"
            ),
            _ => Ok(false),
        }
    }

    /// Whether the lines are in one of these tables.
    pub fn active(&self) -> bool {
        self.in_table.is_some()
    }

    /// One `KEY = value` line of the current table, on line `number`;
    /// `rest` is what follows `=`.
    pub fn entry(&mut self, number: usize, key: &str, rest: &str) -> Result<()> {
        let at = |what: String| format!("dagq.toml:{number}: {what}");
        ensure!(
            !self.keys.iter().any(|seen| seen == key),
            at(format!("{key} is defined twice"))
        );
        self.keys.push(key.to_owned());
        let value = || at(format!("value of {key}"));
        match self.in_table {
            Some(Table::Telemetry) => match key {
                COMMAND_DETAIL => {
                    let text = parse_string(rest).with_context(value)?;
                    self.settings.command_detail =
                        CommandDetail::parse(&text).with_context(|| {
                            at(format!(
                                "{key} is {text:?}; it is one of program, shape and redacted"
                            ))
                        })?;
                }
                FAILED_TESTS => {
                    self.settings.failed_tests = parse_bool(rest).with_context(value)?;
                }
                _ => bail!(at(format!(
                    "unknown key {key} in [{TELEMETRY_TABLE}]; the keys are {COMMAND_DETAIL} and {FAILED_TESTS}"
                ))),
            },
            Some(Table::Command) => {
                let rule = self.open.as_mut().expect("a rule is open");
                let list = || -> Result<Vec<String>> {
                    let items = parse_string_array(rest).with_context(value)?;
                    ensure!(
                        items.iter().all(|item| !item.trim().is_empty()),
                        at(format!("{key} has a blank item"))
                    );
                    Ok(items)
                };
                let flags = || -> Result<Vec<String>> {
                    let items = list()?;
                    for item in &items {
                        ensure!(
                            is_flag(item),
                            at(format!("{key} names {item:?}, which is no flag's name"))
                        );
                    }
                    Ok(items)
                };
                match key {
                    "class" | "group" => {
                        let name = parse_string(rest).with_context(value)?;
                        ensure!(
                            valid_class(&name),
                            at(format!(
                                "{key} {name:?} is not a class's form (lowercase letters, digits, _ and -, a letter first, and not one of the work breakdown's own categories {})",
                                RESERVED_CLASSES.join(", ")
                            ))
                        );
                        if key == "class" {
                            rule.class = name;
                        } else {
                            rule.group = Some(name);
                        }
                    }
                    "program" => {
                        let program = parse_string(rest).with_context(value)?;
                        ensure!(
                            !program.trim().is_empty() && !program.contains(char::is_whitespace),
                            at(format!("{key} {program:?} is no program's name"))
                        );
                        rule.program = program;
                    }
                    "sub" => rule.sub = list()?,
                    "args_any" => rule.args_any = list()?,
                    "flags" => rule.flags = flags()?,
                    "none_of" => rule.none_of = flags()?,
                    "takes_value" => rule.takes_value = flags()?,
                    "lead" => {
                        let words = list()?;
                        for word in &words {
                            ensure!(
                                LEAD_WORDS.contains(&word.as_str()),
                                at(format!(
                                    "{key} names {word:?}, which is no reserved word of the shell"
                                ))
                            );
                        }
                        rule.lead = words;
                    }
                    "positional" => {
                        let text = parse_string(rest).with_context(value)?;
                        rule.positional = Positional::parse(&text)
                            .with_context(|| at(format!("{key} is {text:?}; it is any or none")))?;
                    }
                    "flag_values" => {
                        let values = parse_flag_values(rest).with_context(value)?;
                        for (flag, literals) in &values {
                            ensure!(
                                is_flag(flag),
                                at(format!("{key} names {flag:?}, which is no flag's name"))
                            );
                            ensure!(
                                !literals.is_empty() && literals.iter().all(|l| !l.is_empty()),
                                at(format!("{key} gives {flag} no literal"))
                            );
                        }
                        rule.flag_values = values;
                    }
                    _ => bail!(at(format!(
                        "unknown key {key} in [[{COMMANDS_TABLE}]]; the keys are {}",
                        RULE_KEYS.join(", ")
                    ))),
                }
            }
            None => unreachable!("an entry outside these tables"),
        }
        Ok(())
    }

    /// End the rule being read: it has its class and its program, and is
    /// not one read before.
    fn close(&mut self) -> Result<()> {
        let Some(rule) = self.open.take() else {
            return Ok(());
        };
        let line = rule.line.unwrap_or_default();
        ensure!(
            !rule.class.is_empty(),
            "dagq.toml:{line}: [[{COMMANDS_TABLE}]] has no class"
        );
        ensure!(
            !rule.program.is_empty(),
            "dagq.toml:{line}: [[{COMMANDS_TABLE}]] has no program"
        );
        let same = |other: &CommandRule| CommandRule {
            line: None,
            ..other.clone()
        };
        if let Some(earlier) = self
            .settings
            .commands
            .iter()
            .find(|earlier| same(earlier) == same(&rule))
        {
            bail!(
                "dagq.toml:{line}: [[{COMMANDS_TABLE}]] repeats the rule of line {}",
                earlier.line.unwrap_or_default()
            );
        }
        self.settings.commands.push(rule);
        Ok(())
    }

    /// The settings read, once the file ended.
    pub fn finish(mut self) -> Result<TelemetrySettings> {
        self.close()?;
        Ok(self.settings)
    }
}

/// Whether `name` is a flag's name: `-` and a letter or digit, or `--`
/// and a name, with no `=` or blank.
fn is_flag(name: &str) -> bool {
    let rest = name.strip_prefix("--").or_else(|| name.strip_prefix('-'));
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    })
}

/// A one-line inline table of string arrays (`{ "--test" = ["it"], -m =
/// ['pytest'] }`), its keys bare or quoted, each once, followed by nothing
/// but an optional comment.
fn parse_flag_values(text: &str) -> Result<Vec<(String, Vec<String>)>> {
    let mut rest = text
        .trim()
        .strip_prefix('{')
        .with_context(|| {
            format!("expected an inline table like {{ \"--test\" = [\"it\"] }}, not {text}")
        })?
        .trim_start();
    let mut values: Vec<(String, Vec<String>)> = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            ensure!(
                strip_comment(after).is_empty(),
                "unexpected text after the table: {after}"
            );
            ensure!(!values.is_empty(), "the table is empty");
            return Ok(values);
        }
        let (key, after) = if rest.starts_with(['"', '\'']) {
            let quote = rest.chars().next().expect("a quote");
            let end = rest[1..]
                .find(quote)
                .context("unterminated key in the table")?
                + 2;
            (parse_string(&rest[..end])?, &rest[end..])
        } else {
            let end = rest
                .find(['=', ' '])
                .context("expected KEY = [...] in the table")?;
            (rest[..end].to_owned(), &rest[end..])
        };
        let after = after
            .trim_start()
            .strip_prefix('=')
            .context("expected = after a key of the table")?
            .trim_start();
        ensure!(after.starts_with('['), "expected an array after {key} =");
        let close = array_end(after).context("unterminated array in the table")?;
        let literals = parse_string_array(&after[..=close])?;
        ensure!(
            values.iter().all(|(seen, _)| *seen != key),
            "{key} is in the table twice"
        );
        values.push((key, literals));
        rest = after[close + 1..].trim_start();
        if let Some(next) = rest.strip_prefix(',') {
            rest = next.trim_start();
        } else {
            ensure!(
                rest.starts_with('}'),
                "expected , or }} in the table, not {rest}"
            );
        }
    }
}

/// Where the array `text` starts with ends: its `]` outside its strings.
fn array_end(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        match quote {
            Some(q) => {
                if c == q && !escaped {
                    quote = None;
                }
                escaped = q == '"' && c == '\\' && !escaped;
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == ']' => return Some(at),
            None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests;
