//! The marks of the e2e gate (ADR-t1165-1): `.config/e2e-quarantine.toml`
//! in the checkout the gate runs names the e2e tests whose failure on the
//! rerun is only recorded, and does not fail the gate.
//!
//! ```toml
//! [[test]]
//! name = "up_in_cmux_starts_a_supervisor_in_a_workspace_that_down_wait_stops_and_closes"
//! reason = "the workspace of down --wait is sometimes left open"
//! task = 1120
//! until = 2026-10-15   # or "2026-10-15"; the mark holds through that day
//! ```
//!
//! A mark holds for a test that failed its rerun too, unless it expired,
//! the file has more than [`LIMIT`] marks or cannot be read (then no mark
//! holds), or the test failed the rerun of [`FAILURES_IN_A_ROW`] gates in a
//! row counting this one (it is broken, not flaky).

use serde_json::{Value, json};

/// Where the marks live, relative to the checkout.
pub const FILE: &str = ".config/e2e-quarantine.toml";

/// The most marks a file may have; one with more has none hold.
pub const LIMIT: usize = 3;

/// How many gates in a row (this one included) a marked test may fail its
/// rerun in before its mark stops holding.
pub const FAILURES_IN_A_ROW: usize = 3;

/// One mark of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// The e2e test's name as cargo prints it (`broker::…` for a test of a
    /// module under `tests/e2e/`).
    pub name: String,
    pub reason: String,
    /// The task that fixes the test.
    pub task: i64,
    /// The last day the mark holds (`YYYY-MM-DD`, the host's local date).
    pub until: String,
    /// `until` as days since the epoch.
    pub until_day: i64,
}

impl Mark {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "reason": self.reason, "task": self.task, "until": self.until})
    }
}

/// What the gate found at [`FILE`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum QuarantineFile {
    /// No file: no marks.
    #[default]
    Absent,
    Marks(Vec<Mark>),
    /// It could not be read or parsed: why. No mark holds.
    Unreadable(String),
}

impl QuarantineFile {
    /// The file of `text`.
    pub fn of(text: &str) -> Self {
        match parse(text) {
            Ok(marks) => Self::Marks(marks),
            Err(error) => Self::Unreadable(error),
        }
    }

    fn marks(&self) -> &[Mark] {
        match self {
            Self::Marks(marks) => marks,
            _ => &[],
        }
    }

    /// Why no mark of the file holds, when none does.
    pub fn error(&self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Unreadable(error) => Some(format!("{FILE} could not be read: {error}")),
            Self::Marks(marks) if marks.len() > LIMIT => Some(format!(
                "{FILE} has {} marks, more than {LIMIT}",
                marks.len()
            )),
            Self::Marks(_) => None,
        }
    }
}

/// Why a mark did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ignored {
    /// Its `until` is past.
    Expired { until: String },
    /// The file has more than [`LIMIT`] marks.
    OverLimit { marks: usize },
    /// Its test failed the rerun of this many gates in a row, this one
    /// included.
    FailedInARow { gates: usize },
}

impl Ignored {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Expired { .. } => "expired",
            Self::OverLimit { .. } => "over_limit",
            Self::FailedInARow { .. } => "failed_in_a_row",
        }
    }

    pub fn sentence(&self) -> String {
        match self {
            Self::Expired { until } => format!("its mark expired after {until}"),
            Self::OverLimit { marks } => {
                format!("{FILE} has {marks} marks, more than {LIMIT}")
            }
            Self::FailedInARow { gates } => format!(
                "it failed the rerun of {gates} gates in a row (a mark holds for fewer than \
{FAILURES_IN_A_ROW}), so it is broken rather than flaky"
            ),
        }
    }
}

/// How the marks judged the tests that failed their rerun.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judged {
    /// The tests that failed the rerun under a mark that holds.
    pub quarantined: Vec<String>,
    /// The tests that failed the rerun under no mark that holds.
    pub unmarked: Vec<String>,
    /// The marks that did not hold, and why.
    pub ignored: Vec<(String, Ignored)>,
}

/// Judge `still_failing` (the tests that failed their rerun) by `file` on
/// the local day `today`; `in_a_row` gives how many gates right before this
/// one a test failed its rerun in.
pub fn judge(
    file: &QuarantineFile,
    still_failing: &[String],
    today: i64,
    in_a_row: &dyn Fn(&str) -> usize,
) -> Judged {
    let marks = file.marks();
    let mut ignored = Vec::new();
    let mut holding: Vec<&str> = Vec::new();
    for mark in marks {
        let failing = still_failing.contains(&mark.name);
        let why = if marks.len() > LIMIT {
            Some(Ignored::OverLimit { marks: marks.len() })
        } else if mark.until_day < today {
            Some(Ignored::Expired {
                until: mark.until.clone(),
            })
        } else if failing && in_a_row(&mark.name) + 1 >= FAILURES_IN_A_ROW {
            Some(Ignored::FailedInARow {
                gates: in_a_row(&mark.name) + 1,
            })
        } else {
            None
        };
        match why {
            Some(why) => ignored.push((mark.name.clone(), why)),
            None => holding.push(&mark.name),
        }
    }
    let (quarantined, unmarked) = still_failing
        .iter()
        .cloned()
        .partition(|test| holding.contains(&test.as_str()));
    Judged {
        quarantined,
        unmarked,
        ignored,
    }
}

/// The marks of the file's `text`, or why it cannot be read.
pub fn parse(text: &str) -> Result<Vec<Mark>, String> {
    #[derive(Default)]
    struct Draft {
        line: usize,
        name: Option<String>,
        reason: Option<String>,
        task: Option<i64>,
        until: Option<(String, i64)>,
    }
    fn finish(draft: Draft) -> Result<Mark, String> {
        let missing = |key: &str| format!("the [[test]] of line {} has no {key}", draft.line);
        let (until, until_day) = draft.until.ok_or_else(|| missing("until"))?;
        Ok(Mark {
            name: draft.name.ok_or_else(|| missing("name"))?,
            reason: draft.reason.ok_or_else(|| missing("reason"))?,
            task: draft.task.ok_or_else(|| missing("task"))?,
            until,
            until_day,
        })
    }
    let mut marks: Vec<Mark> = Vec::new();
    let mut draft: Option<Draft> = None;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let header = strip_comment(line);
            if header != "[[test]]" {
                return Err(format!(
                    "line {number}: only [[test]] tables are known, not {header}"
                ));
            }
            if let Some(done) = draft.take() {
                marks.push(finish(done)?);
            }
            draft = Some(Draft {
                line: number,
                ..Draft::default()
            });
            continue;
        }
        let Some(current) = draft.as_mut() else {
            return Err(format!("line {number}: a key outside a [[test]] table"));
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {number}: expected KEY = value"));
        };
        let key = key.trim();
        let value = value.trim();
        let at = |error: String| format!("line {number}: {key}: {error}");
        let duplicate = || format!("line {number}: {key} is given twice");
        match key {
            "name" | "reason" => {
                let text = string(value).map_err(at)?;
                if text.is_empty() {
                    return Err(at("is empty".to_owned()));
                }
                let slot = if key == "name" {
                    &mut current.name
                } else {
                    &mut current.reason
                };
                if slot.replace(text).is_some() {
                    return Err(duplicate());
                }
            }
            "task" => {
                let task = strip_comment(value)
                    .parse::<i64>()
                    .ok()
                    .filter(|task| *task > 0)
                    .ok_or_else(|| at(format!("expected a task ID, not {value}")))?;
                if current.task.replace(task).is_some() {
                    return Err(duplicate());
                }
            }
            "until" => {
                let bare = strip_comment(value);
                let text = if bare.starts_with(['"', '\'']) {
                    string(value).map_err(at)?
                } else {
                    bare.to_owned()
                };
                let day = date(&text)
                    .ok_or_else(|| at(format!("expected a date YYYY-MM-DD, not {text}")))?;
                if current.until.replace((text, day)).is_some() {
                    return Err(duplicate());
                }
            }
            _ => return Err(format!("line {number}: unknown key {key}")),
        }
    }
    if let Some(done) = draft.take() {
        marks.push(finish(done)?);
    }
    for (index, mark) in marks.iter().enumerate() {
        if marks[..index].iter().any(|other| other.name == mark.name) {
            return Err(format!("{} is marked twice", mark.name));
        }
    }
    Ok(marks)
}

/// `text` without a trailing `# comment`, trimmed (for values without
/// quotes).
fn strip_comment(text: &str) -> &str {
    text.split_once('#')
        .map_or(text, |(before, _)| before)
        .trim()
}

/// The string `value` quotes (`"…"` with `\"`, `\\`, `\n`, `\t`, or
/// `'…'` literally), followed by nothing but a comment.
fn string(value: &str) -> Result<String, String> {
    let mut chars = value.chars();
    let quote = chars
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))
        .ok_or_else(|| format!("expected a quoted string, not {value}"))?;
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err(format!("the string {value} is not closed")),
            Some(c) if c == quote => break,
            Some('\\') if quote == '"' => match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                other => return Err(format!("unknown escape \\{}", other.unwrap_or(' '))),
            },
            Some(c) => out.push(c),
        }
    }
    let rest = chars.as_str().trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(format!("unexpected {rest} after the string"));
    }
    Ok(out)
}

/// The days since the epoch of `YYYY-MM-DD`.
fn date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = &text[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days_in_month)
        .contains(&day)
        .then(|| super::host_metrics::days_from_civil(year, month, day))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(name: &str, until: &str) -> String {
        format!("[[test]]\nname = \"{name}\"\nreason = \"flaky\"\ntask = 7\nuntil = {until}\n")
    }

    #[test]
    fn the_marks_are_read_with_their_fields() {
        let text = "# the marks\n\n[[test]]  # one\nname = \"broker::lands\"  # its name\n\
reason = 'a \"quoted\" reason'\ntask = 1120\nuntil = 2026-10-15 # the day\n\n[[test]]\n\
name = \"b\"\nreason = \"x\\ty\"\ntask = 3\nuntil = \"2024-02-29\"\n";
        let marks = parse(text).unwrap();
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].name, "broker::lands");
        assert_eq!(marks[0].reason, "a \"quoted\" reason");
        assert_eq!(marks[0].task, 1120);
        assert_eq!(marks[0].until, "2026-10-15");
        assert_eq!(marks[0].until_day, date("2026-10-15").unwrap());
        assert_eq!(marks[1].reason, "x\ty");
        assert_eq!(marks[1].until, "2024-02-29");
        assert_eq!(
            marks[0].to_json(),
            json!({"name": "broker::lands", "reason": "a \"quoted\" reason", "task": 1120, "until": "2026-10-15"})
        );
        assert_eq!(parse("").unwrap(), []);
        assert_eq!(
            QuarantineFile::of("# none\n"),
            QuarantineFile::Marks(vec![])
        );
        assert_eq!(date("1970-01-02"), Some(1));
    }

    #[test]
    fn a_file_that_cannot_be_read_says_where() {
        for (text, expected) in [
            ("[test]\n", "only [[test]] tables"),
            ("name = \"a\"\n", "outside a [[test]]"),
            ("[[test]]\nname\n", "expected KEY = value"),
            ("[[test]]\nwho = 1\n", "unknown key who"),
            ("[[test]]\nname = a\n", "expected a quoted string"),
            ("[[test]]\nname = \"a\n", "not closed"),
            ("[[test]]\nname = \"a\" b\n", "unexpected b"),
            ("[[test]]\nname = \"\\q\"\n", "unknown escape"),
            ("[[test]]\nname = \"\"\n", "is empty"),
            ("[[test]]\nname = \"a\"\nname = \"b\"\n", "given twice"),
            ("[[test]]\ntask = x\n", "expected a task ID"),
            ("[[test]]\ntask = 0\n", "expected a task ID"),
            ("[[test]]\ntask = 1\ntask = 2\n", "given twice"),
            ("[[test]]\nuntil = 2026-02-30\n", "expected a date"),
            ("[[test]]\nuntil = 2026-13-01\n", "expected a date"),
            ("[[test]]\nuntil = 26-10-15\n", "expected a date"),
            ("[[test]]\nuntil = 2026-1a-01\n", "expected a date"),
            ("[[test]]\nuntil = \"2026-10-15\nx\"\n", "not closed"),
            (
                "[[test]]\nuntil = 2026-10-15\nuntil = 2026-10-16\n",
                "given twice",
            ),
            ("[[test]]\nname = \"a\"\n", "has no until"),
            (
                &format!("{}{}", mark("a", "2026-10-15"), mark("a", "2026-10-16")),
                "marked twice",
            ),
        ] {
            let error = parse(text).unwrap_err();
            assert!(error.contains(expected), "{text:?}: {error}");
        }
        let file = QuarantineFile::of("[x]\n");
        assert!(
            file.error()
                .unwrap()
                .starts_with(".config/e2e-quarantine.toml could not be read: line 1"),
            "{file:?}"
        );
        assert_eq!(QuarantineFile::Absent.error(), None);
    }

    #[test]
    fn a_mark_holds_unless_expired_over_the_limit_or_failing_in_a_row() {
        let today = date("2026-10-01").unwrap();
        let file = QuarantineFile::of(&format!(
            "{}{}{}",
            mark("held", "2026-10-01"),
            mark("expired", "2026-09-30"),
            mark("broken", "2026-12-31")
        ));
        assert_eq!(file.error(), None);
        let failing: Vec<String> = ["held", "expired", "broken", "plain"]
            .map(String::from)
            .to_vec();
        let in_a_row = |test: &str| if test == "broken" { 2 } else { 1 };
        let judged = judge(&file, &failing, today, &in_a_row);
        assert_eq!(judged.quarantined, ["held"]);
        assert_eq!(judged.unmarked, ["expired", "broken", "plain"]);
        assert_eq!(
            judged.ignored,
            [
                (
                    "expired".to_owned(),
                    Ignored::Expired {
                        until: "2026-09-30".into()
                    }
                ),
                ("broken".to_owned(), Ignored::FailedInARow { gates: 3 }),
            ]
        );
        assert_eq!(judged.ignored[0].1.code(), "expired");
        assert!(judged.ignored[1].1.sentence().contains("3 gates in a row"));
        assert_eq!(judged.ignored[1].1.code(), "failed_in_a_row");

        // A mark whose test passed is not broken, only expired or not.
        let judged = judge(&file, &[], today, &|_| 5);
        assert_eq!(judged.ignored.len(), 1, "{judged:?}");

        // Over the limit, no mark holds.
        let file = QuarantineFile::of(&format!(
            "{}{}{}{}",
            mark("a", "2026-12-31"),
            mark("b", "2026-12-31"),
            mark("c", "2026-12-31"),
            mark("d", "2026-12-31")
        ));
        assert!(file.error().unwrap().contains("4 marks, more than 3"));
        let judged = judge(&file, &["a".to_owned()], today, &|_| 0);
        assert_eq!(judged.unmarked, ["a"]);
        assert_eq!(judged.ignored.len(), 4);
        assert_eq!(judged.ignored[0].1.code(), "over_limit");
        assert!(judged.ignored[0].1.sentence().contains("more than 3"));
        assert!(
            Ignored::Expired {
                until: "2026-09-30".into()
            }
            .sentence()
            .contains("expired after 2026-09-30")
        );

        // An unreadable file has no marks.
        let judged = judge(&QuarantineFile::of("[x]"), &["a".to_owned()], today, &|_| 0);
        assert_eq!(judged.unmarked, ["a"]);
        assert!(judged.ignored.is_empty());
    }
}
