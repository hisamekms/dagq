//! A project's own marks on its verification's output (ADR-t2268-1
//! decision 2): a line of the project's own tools or test helpers that
//! puts a failure down to one of the classes the runtime already has, and
//! may name the test that failed. A repository declares them in
//! `dagq.toml` (the format is on the table's definition in the config's
//! reader); [`crate::domain::verify_failure::classify`] tries them in the
//! order it keeps for a profile's marks.
//!
//! A mark adds a line to an existing class and never a class: what it may
//! name is [`MarkClass`], the host's classes a profile may name and the
//! code's (ADR-t2268-1 decision 4, ADR-t639-1 decision 5).
use super::{
    toolchain::{ToolCodeClass, ToolHostClass},
    verify_failure::test_name,
};

/// The class a project's mark puts a failure down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkClass {
    /// One of the host's, tried where a profile's test out of time is: after
    /// the runtime's full disk and signal, and the flaky tests, before the
    /// exit 124.
    Host(ToolHostClass),
    /// One of the code's, tried before the profiles' code failures.
    Code(ToolCodeClass),
}

/// The classes a mark may name, by the name `dagq.toml` writes.
const CLASSES: [(&str, MarkClass); 7] = [
    ("killed", MarkClass::Host(ToolHostClass::Killed)),
    ("timeout", MarkClass::Host(ToolHostClass::Timeout)),
    ("build_error", MarkClass::Code(ToolCodeClass::BuildError)),
    ("lint", MarkClass::Code(ToolCodeClass::Lint)),
    ("test_failure", MarkClass::Code(ToolCodeClass::TestFailure)),
    ("format", MarkClass::Code(ToolCodeClass::Format)),
    (
        "coverage_below",
        MarkClass::Code(ToolCodeClass::CoverageBelow),
    ),
];

impl MarkClass {
    /// The class named `name`; `disk_full` (the runtime's own errno),
    /// `flaky` (a tool's retry) and `unknown` (no mark) take no mark, and
    /// any other name is none of the runtime's.
    pub fn parse(name: &str) -> Result<Self, String> {
        CLASSES
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, class)| *class)
            .ok_or_else(|| {
                let names = CLASSES.map(|(known, _)| known).join(", ");
                format!("unknown class {name:?}; a mark names one of {names}")
            })
    }

    pub fn as_str(self) -> &'static str {
        CLASSES
            .iter()
            .find(|(_, class)| *class == self)
            .map(|(name, _)| *name)
            .expect("every class has its name")
    }
}

/// One piece of a [`LinePattern`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Any,
    Test,
}

/// The shape of a mark's line: the whole line, trimmed, with `*` for any
/// text (none too) and at most one `{test}` for the failed test's name;
/// the rest is the line's own text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePattern {
    source: String,
    pieces: Vec<Piece>,
}

/// The placeholder of the test's name in a [`LinePattern`].
const TEST: &str = "{test}";

impl LinePattern {
    /// `source` read as a pattern; one with no text of its own, or with
    /// `{test}` twice or beside `*` or another `{test}`, is a mistake.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut pieces: Vec<Piece> = Vec::new();
        let mut rest = source.trim();
        while !rest.is_empty() {
            let (piece, len) = if rest.starts_with(TEST) {
                (Piece::Test, TEST.len())
            } else if rest.starts_with('*') {
                (Piece::Any, 1)
            } else {
                let end = rest
                    .find('*')
                    .into_iter()
                    .chain(rest.find(TEST))
                    .min()
                    .unwrap_or(rest.len());
                (Piece::Text(rest[..end].to_owned()), end)
            };
            if matches!(
                (pieces.last(), &piece),
                (Some(Piece::Test), Piece::Any | Piece::Test) | (Some(Piece::Any), Piece::Test)
            ) {
                return Err(format!(
                    "{TEST} is next to `*` or another {TEST} in {source:?}"
                ));
            }
            if !(piece == Piece::Any && pieces.last() == Some(&Piece::Any)) {
                pieces.push(piece);
            }
            rest = &rest[len..];
        }
        if pieces.iter().filter(|piece| **piece == Piece::Test).count() > 1 {
            return Err(format!("{TEST} comes more than once in {source:?}"));
        }
        if !pieces
            .iter()
            .any(|piece| matches!(piece, Piece::Text(text) if !text.trim().is_empty()))
        {
            return Err(format!("{source:?} has no text of the line"));
        }
        Ok(Self {
            source: source.trim().to_owned(),
            pieces,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether `line` (trimmed) has the shape, and the text in `{test}`'s
    /// place, the shortest that fits.
    ///
    /// Each wildcard (`*` or `{test}`) takes the least it can: the text
    /// after it is placed where it first comes, and the last text is held
    /// to the line's end. Placing each text at its first place leaves the
    /// most room for the rest, so this finds a match whenever one exists,
    /// and the one where every wildcard, in order, is the shortest; each
    /// text is searched once, so the time is linear in the line.
    fn matches<'a>(&self, line: &'a str) -> Option<Option<&'a str>> {
        let mut pieces = self.pieces.as_slice();
        // The text before the first wildcard starts the line.
        let mut at = 0;
        if let Some((Piece::Text(text), rest)) = pieces.split_first() {
            line.strip_prefix(text.as_str())?;
            at = text.len();
            pieces = rest;
        }
        let Some((last, middle)) = pieces.split_last() else {
            return (at == line.len()).then_some(None);
        };
        // The text after the last wildcard ends the line, after `at`.
        let end = match last {
            Piece::Text(text) => {
                let end = line.len().checked_sub(text.len())?;
                (end >= at && line.ends_with(text.as_str())).then_some(end)?
            }
            Piece::Any | Piece::Test => line.len(),
        };
        let mut test = None;
        let mut test_from = None;
        for piece in middle {
            match piece {
                Piece::Text(text) => {
                    let found = at + line[at..end].find(text.as_str())?;
                    if let Some(from) = test_from.take() {
                        test = Some(&line[from..found]);
                    }
                    at = found + text.len();
                }
                Piece::Test => test_from = Some(at),
                Piece::Any => {}
            }
        }
        if *last == Piece::Test {
            test_from = Some(at);
        }
        if let Some(from) = test_from {
            test = Some(&line[from..end]);
        }
        Some(test)
    }
}

/// A `[verify_marks.<name>]` of `dagq.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMark {
    /// The table's `<name>`.
    pub name: String,
    pub class: MarkClass,
    pub line: LinePattern,
}

impl ProjectMark {
    /// Whether `line` (trimmed) is the mark's.
    pub fn is_on(&self, line: &str) -> bool {
        self.line.matches(line).is_some()
    }

    /// The test `line` (trimmed) names, when it is the mark's and the text
    /// in `{test}`'s place looks like a test's name.
    pub fn test_on(&self, line: &str) -> Option<String> {
        self.line.matches(line).flatten().and_then(test_name)
    }
}

/// The timeout line of this repository's own test helper (`tests/common`'s
/// `within`) as its `dagq.toml` declares it, for the tests that read this
/// repository's output.
#[cfg(test)]
pub(crate) fn within() -> ProjectMark {
    ProjectMark {
        name: "within".to_owned(),
        class: MarkClass::Host(ToolHostClass::Timeout),
        line: LinePattern::parse("test {test} timed out: *did not happen within*")
            .expect("the pattern is well formed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(source: &str) -> LinePattern {
        LinePattern::parse(source).unwrap()
    }

    #[test]
    fn a_class_is_one_the_runtime_has() {
        for (name, _) in CLASSES {
            assert_eq!(MarkClass::parse(name).unwrap().as_str(), name);
        }
        for name in ["disk_full", "flaky", "unknown", "slow", ""] {
            let error = MarkClass::parse(name).err().unwrap();
            assert!(
                error.starts_with(&format!("unknown class {name:?}")),
                "{error}"
            );
        }
    }

    #[test]
    fn a_pattern_reads_the_whole_line_and_the_shortest_test() {
        let within = within();
        let line = "test runtime_x::waits timed out: the run to land did not happen within 20s";
        assert!(within.is_on(line));
        assert_eq!(within.test_on(line).as_deref(), Some("runtime_x::waits"));
        // The quoted text of an assertion does not start the line.
        assert!(!within.is_on("stderr: \"test a::b timed out: x did not happen within 1s\""));
        assert!(!within.is_on("test a::b timed out: it did happen"));
        // The line is the mark's, but the text is no test's name.
        let odd = "test a b timed out: x did not happen within 1s";
        assert!(within.is_on(odd));
        assert_eq!(within.test_on(odd), None);
        // Without `*` the line ends with the text.
        let exact = pattern("FAILED {test}");
        assert_eq!(exact.matches("FAILED a::b"), Some(Some("a::b")));
        let tail = pattern("abort*");
        assert_eq!(tail.matches("abort: x"), Some(None));
        assert_eq!(tail.matches("x abort"), None);
    }

    /// The match tried at every place of every wildcard, shortest first:
    /// the meaning [`LinePattern::matches`] keeps, for small lines only.
    fn every_place<'a>(pieces: &[Piece], line: &'a str) -> Option<Option<&'a str>> {
        let Some((first, rest)) = pieces.split_first() else {
            return line.is_empty().then_some(None);
        };
        match first {
            Piece::Text(text) => every_place(rest, line.strip_prefix(text.as_str())?),
            Piece::Any | Piece::Test => line
                .char_indices()
                .map(|(at, _)| at)
                .chain([line.len()])
                .find_map(|at| {
                    every_place(rest, &line[at..]).map(|inner| match first {
                        Piece::Test => Some(&line[..at]),
                        _ => inner,
                    })
                }),
        }
    }

    #[test]
    fn a_pattern_matches_as_trying_every_place_would() {
        let sources = [
            "a",
            "ab",
            "a*",
            "*a",
            "*a*",
            "a*b",
            "a*a",
            "ab*ab",
            "a{test}b",
            "{test}a",
            "a{test}",
            "*a{test}b*",
            "a*b{test}a*",
            "*ab*b{test}b",
            "b{test}a*a*b",
            "*a*b*a*",
        ];
        // Every line of up to 6 letters from `a`, `b` and `é`.
        let mut lines = vec![String::new()];
        let mut last = vec![String::new()];
        for _ in 0..6 {
            last = last
                .iter()
                .flat_map(|line| ['a', 'b', 'é'].map(|c| format!("{line}{c}")))
                .collect();
            lines.extend(last.iter().cloned());
        }
        for source in sources {
            let pattern = pattern(source);
            for line in &lines {
                assert_eq!(
                    pattern.matches(line),
                    every_place(&pattern.pieces, line),
                    "{source:?} on {line:?}"
                );
            }
        }
    }

    #[test]
    fn many_wildcards_on_a_long_near_line_end_at_once() {
        // Trying every place of each of the 5 wildcards would take about
        // n^5 steps on these lines; the match reads each line a few times.
        let n = 50_000;
        let many = pattern("start * x * x * {test} y * x * end");
        let near = format!("start {} end", " x".repeat(n));
        assert_eq!(many.matches(&near), None);
        let near_end = format!("start {} x y x en", " x".repeat(n));
        assert_eq!(many.matches(&near_end), None);
        // The wildcards before `{test}` take the first ` x ` they can.
        let line = format!("start {} a::b y q x q end", " x".repeat(n));
        let test = many.matches(&line).flatten().unwrap();
        assert_eq!(test, &line[15..line.find(" y ").unwrap()]);
        assert!(test.ends_with("x x a::b"));
    }

    #[test]
    fn a_pattern_without_text_or_with_test_twice_is_a_mistake() {
        for source in [
            "",
            "*",
            "{test}",
            " * {test}",
            "a {test}{test} b",
            "a {test}* b",
            "a *{test} b",
            "a {test} b {test}",
        ] {
            assert!(LinePattern::parse(source).is_err(), "{source:?}");
        }
        assert_eq!(pattern("a ** b").pieces.len(), 3);
        assert_eq!(pattern("  run {test} hung  ").as_str(), "run {test} hung");
    }
}
