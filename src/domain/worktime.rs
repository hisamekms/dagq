//! The work of a run's own session (worker, resume, revise) broken down by
//! what it spent its time on (task 514): the model thinking and writing,
//! the commands it ran by kind, its subagents, its waits, and idle time.
//! Derived from the session's transcript over the span, by fixed rules;
//! reading the transcript is the infrastructure's, this module is pure.
//!
//! Every millisecond of the span gets one category, by priority:
//! a foreground tool > a background command > a subagent > the model >
//! idle. A shell command is labelled by the heaviest thing it runs
//! ([`classify`]); a background command and an async subagent end at their
//! `<task-notification>`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::transcript::{RecordRole, TranscriptRecord, millis_text};

pub const MODEL: &str = "model";
pub const E2E: &str = "e2e";
pub const LLVM_COV: &str = "llvm_cov";
pub const TEST: &str = "test";
pub const BUILD: &str = "build";
pub const FMT: &str = "fmt";
pub const WAIT: &str = "wait";
pub const DAGQ: &str = "dagq";
pub const GIT: &str = "git";
pub const OTHER_COMMAND: &str = "other_command";
/// Commands joined with `&&`, `||`, `;`, `&` or a newline that run two or more
/// heavy kinds (say fmt, clippy and llvm-cov in one line).
pub const CHAIN: &str = "chain";
/// Other tools (reading, editing, searching files).
pub const TOOL: &str = "tool";
pub const SUBAGENT: &str = "subagent";
pub const IDLE: &str = "idle";

/// Every category, in the order they are listed, which also breaks ties
/// between overlapping commands of the same priority.
pub const CATEGORIES: [&str; 14] = [
    MODEL,
    CHAIN,
    E2E,
    LLVM_COV,
    TEST,
    BUILD,
    FMT,
    WAIT,
    DAGQ,
    GIT,
    OTHER_COMMAND,
    TOOL,
    SUBAGENT,
    IDLE,
];

/// The heavy commands: counted and failed per kind, and listed in
/// `timeline`.
pub const HEAVY: [&str; 5] = [CHAIN, E2E, LLVM_COV, TEST, BUILD];

/// The categories of commands that run tests, whose output is read for
/// the failed tests (task 515).
pub const TEST_KINDS: [&str; 4] = [CHAIN, E2E, LLVM_COV, TEST];

/// The most failed test names one span's `work` keeps (task 515).
pub const MAX_SPAN_FAILED_TESTS: usize = 50;

/// Tools that only wait (for a wakeup, a monitor, a background output).
const WAIT_TOOLS: [&str; 4] = ["ScheduleWakeup", "Monitor", "TaskOutput", "BashOutput"];

/// The category of a shell `command`: the heaviest of its parts, or
/// [`CHAIN`] when two or more parts are heavy of different kinds.
pub fn classify(command: &str) -> &'static str {
    let mut kinds: Vec<&'static str> = parts(command).iter().map(|part| rank(part)).collect();
    kinds.sort_unstable_by_key(|kind| RANKS.iter().position(|k| k == kind));
    kinds.dedup();
    if kinds.iter().filter(|kind| HEAVY.contains(kind)).count() >= 2 {
        return CHAIN;
    }
    kinds.first().copied().unwrap_or(OTHER_COMMAND)
}

/// The kinds of a part, heaviest first.
const RANKS: [&str; 9] = [
    E2E,
    LLVM_COV,
    TEST,
    BUILD,
    FMT,
    WAIT,
    DAGQ,
    GIT,
    OTHER_COMMAND,
];

/// A word of a shell command: its text without the quotes and escapes,
/// and where its first quoted character is (`None` when none is).
#[derive(Debug, Default)]
struct Word {
    text: String,
    quoted_from: Option<usize>,
}

impl Word {
    fn quote(&mut self) {
        self.quoted_from.get_or_insert(self.text.len());
    }
}

/// The simple commands of one part: split at a pipe, and at a subshell's
/// or a command substitution's bounds (`(`, `$(`, `` ` ``, `)`).
type Part = Vec<Vec<Word>>;

/// The parts of a command, split at `&&`, `||`, `;`, `&` and newlines
/// outside quotes. Quoted text stays in its word, the body of a heredoc
/// (`<<EOF`, `<<'EOF'`, `<<-EOF`) and a `#` comment are dropped.
fn parts(command: &str) -> Vec<Part> {
    Splitter::default().split(command)
}

#[derive(Default)]
struct Splitter {
    parts: Vec<Part>,
    part: Part,
    simple: Vec<Word>,
    word: Option<Word>,
    /// The simple commands a `(`, `$(` or `` ` `` suspended, and whether
    /// the substitution was in double quotes (`"$(…)"`), which go on after
    /// its `)`.
    outer: Vec<(Vec<Word>, bool)>,
    in_backtick: bool,
    /// The delimiters of the heredocs whose body starts at the next
    /// newline, and whether leading tabs are stripped (`<<-`).
    heredocs: Vec<(String, bool)>,
}

impl Splitter {
    fn split(mut self, command: &str) -> Vec<Part> {
        let chars: Vec<char> = command.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            match c {
                '\n' => {
                    self.end_part();
                    i = self.skip_heredocs(&chars, i + 1);
                    continue;
                }
                ' ' | '\t' | '\r' => self.end_word(),
                '#' if self.word.is_none() => {
                    while i + 1 < chars.len() && chars[i + 1] != '\n' {
                        i += 1;
                    }
                }
                '\'' => {
                    let word = self.word.get_or_insert_default();
                    word.quote();
                    i += 1;
                    while i < chars.len() && chars[i] != '\'' {
                        word.text.push(chars[i]);
                        i += 1;
                    }
                }
                '"' => {
                    i = self.double_quoted(&chars, i + 1);
                    continue;
                }
                '\\' => {
                    if next == Some('\n') {
                        i += 1;
                    } else if let Some(escaped) = next {
                        let word = self.word.get_or_insert_default();
                        // `\cargo` still runs cargo; `\;` is no separator.
                        if !escaped.is_alphanumeric() {
                            word.quote();
                        }
                        word.text.push(escaped);
                        i += 1;
                    }
                }
                '&' if next == Some('&') => {
                    self.end_part();
                    i += 1;
                }
                '&' if next == Some('>')
                    || self
                        .word
                        .as_ref()
                        .is_some_and(|w| w.text.ends_with(['>', '<'])) =>
                {
                    self.word.get_or_insert_default().text.push(c);
                }
                '&' | ';' => self.end_part(),
                '|' if next == Some('|') => {
                    self.end_part();
                    i += 1;
                }
                '|' => {
                    self.end_simple();
                    if next == Some('&') {
                        i += 1;
                    }
                }
                // An arithmetic `$((…))` runs nothing and has no heredoc.
                '(' if next == Some('(')
                    && self
                        .word
                        .as_ref()
                        .is_some_and(|w| w.quoted_from.is_none() && w.text.ends_with('$')) =>
                {
                    let word = self.word.get_or_insert_default();
                    let mut depth = 0;
                    while let Some(&c) = chars.get(i) {
                        depth += match c {
                            '(' => 1,
                            ')' => -1,
                            _ => 0,
                        };
                        word.text.push(c);
                        i += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    continue;
                }
                '(' => {
                    if let Some(word) = self.word.as_mut()
                        && word.quoted_from.is_none()
                        && word.text.ends_with('$')
                    {
                        word.text.pop();
                    }
                    self.open(false);
                }
                ')' => {
                    if self.close() {
                        i = self.double_quoted(&chars, i + 1);
                        continue;
                    }
                }
                '`' if self.in_backtick => {
                    self.in_backtick = false;
                    self.close();
                }
                '`' => {
                    self.in_backtick = true;
                    self.open(false);
                }
                '<' if next == Some('<') && chars.get(i + 2) == Some(&'<') => {
                    self.word.get_or_insert_default().text.push_str("<<<");
                    i += 2;
                }
                '<' if next == Some('<') => {
                    self.end_word();
                    i = self.heredoc(&chars, i + 2);
                    continue;
                }
                _ => self.word.get_or_insert_default().text.push(c),
            }
            i += 1;
        }
        self.end_word();
        while let Some((outer, _)) = self.outer.pop() {
            self.end_simple();
            self.simple = outer;
        }
        self.end_part();
        self.parts
    }

    fn end_word(&mut self) {
        if let Some(word) = self.word.take()
            && (!word.text.is_empty() || word.quoted_from.is_some())
        {
            self.simple.push(word);
        }
    }

    fn end_simple(&mut self) {
        self.end_word();
        if !self.simple.is_empty() {
            self.part.push(std::mem::take(&mut self.simple));
        }
    }

    fn end_part(&mut self) {
        self.end_simple();
        if !self.part.is_empty() {
            self.parts.push(std::mem::take(&mut self.part));
        }
    }

    fn open(&mut self, in_double: bool) {
        self.end_word();
        self.outer
            .push((std::mem::take(&mut self.simple), in_double));
    }

    /// Ends a substitution or a subshell; whether it was in double quotes.
    fn close(&mut self) -> bool {
        self.end_simple();
        let (outer, in_double) = self.outer.pop().unwrap_or_default();
        self.simple = outer;
        in_double
    }

    /// Reads double-quoted text from `at` into the word, up to the closing
    /// quote or a `$(`, whose commands run (and are read as commands);
    /// returns where the command goes on.
    fn double_quoted(&mut self, chars: &[char], mut at: usize) -> usize {
        self.word.get_or_insert_default().quote();
        while let Some(&c) = chars.get(at) {
            let next = chars.get(at + 1).copied();
            match c {
                '"' => return at + 1,
                '\\' if matches!(next, Some('"' | '\\' | '$' | '`')) => {
                    self.word.get_or_insert_default().text.extend(next);
                    at += 2;
                }
                '$' if next == Some('(') && chars.get(at + 2) != Some(&'(') => {
                    self.open(true);
                    return at + 2;
                }
                _ => {
                    self.word.get_or_insert_default().text.push(c);
                    at += 1;
                }
            }
        }
        at
    }

    /// Reads a heredoc's delimiter from `at` (after `<<`); returns where
    /// the rest of the line goes on.
    fn heredoc(&mut self, chars: &[char], mut at: usize) -> usize {
        let strip_tabs = chars.get(at) == Some(&'-');
        if strip_tabs {
            at += 1;
        }
        while matches!(chars.get(at), Some(' ' | '\t')) {
            at += 1;
        }
        let mut delimiter = String::new();
        while let Some(&c) = chars.get(at) {
            if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
                break;
            }
            if !matches!(c, '\'' | '"' | '\\') {
                delimiter.push(c);
            }
            at += 1;
        }
        if !delimiter.is_empty() {
            self.heredocs.push((delimiter, strip_tabs));
        }
        at
    }

    /// Skips the bodies of the pending heredocs, from the line at `at`;
    /// returns where the command goes on.
    fn skip_heredocs(&mut self, chars: &[char], mut at: usize) -> usize {
        for (delimiter, strip_tabs) in std::mem::take(&mut self.heredocs) {
            while at < chars.len() {
                let end = chars[at..]
                    .iter()
                    .position(|&c| c == '\n')
                    .map_or(chars.len(), |n| at + n);
                let line: String = chars[at..end].iter().collect();
                let line = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                at = (end + 1).min(chars.len());
                if line.trim_end() == delimiter {
                    break;
                }
            }
        }
        at
    }
}

/// What a simple command runs: the keywords and prefixes before it
/// (`until`, `time`, `env`...), the name of the command word (its file
/// name), and its arguments. Assignments (`VAR=x`) before it are skipped;
/// a quoted word is not a command.
struct Run<'a> {
    lead: Vec<&'a str>,
    name: &'a str,
    args: &'a [Word],
}

const KEYWORDS: [&str; 11] = [
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "}", "done",
];

/// Commands that run the command after them, with their flags that take
/// a value in the next word; `timeout` also takes a duration.
const PREFIXES: [(&str, &[&str]); 10] = [
    ("time", &[]),
    ("nohup", &[]),
    ("command", &[]),
    ("exec", &["-a"]),
    (
        "env",
        &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"],
    ),
    ("timeout", &["-s", "--signal", "-k", "--kill-after"]),
    ("nice", &["-n", "--adjustment"]),
    (
        "sudo",
        &["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U"],
    ),
    (
        "xargs",
        &[
            "-n", "-I", "-J", "-L", "-P", "-R", "-S", "-d", "-E", "-s", "-a",
        ],
    ),
    ("caffeinate", &["-t", "-w"]),
];

fn run(simple: &[Word]) -> Option<Run<'_>> {
    let mut lead = Vec::new();
    let mut at = 0;
    while let Some(word) = simple.get(at) {
        at += 1;
        if assignment(word) {
            continue;
        }
        if word.quoted_from.is_some() {
            return None;
        }
        let name = word.text.rsplit('/').next().unwrap_or_default();
        if KEYWORDS.contains(&name) {
            lead.push(name);
        } else if let Some((_, valued)) = PREFIXES.iter().find(|(prefix, _)| *prefix == name) {
            lead.push(name);
            while let Some(flag) = simple.get(at).filter(|w| w.text.starts_with('-')) {
                at += 1;
                if valued.contains(&flag.text.as_str()) {
                    at += 1;
                }
            }
            if name == "timeout" {
                at += 1;
            }
        } else {
            return Some(Run {
                lead,
                name,
                args: &simple[at..],
            });
        }
    }
    None
}

/// Whether a word is a shell assignment (`NAME=value`), its name unquoted.
fn assignment(word: &Word) -> bool {
    let Some(eq) = word.text.find('=') else {
        return false;
    };
    let name = &word.text[..eq];
    word.quoted_from.is_none_or(|from| from > eq)
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The first `cargo` a part runs, and its subcommand (a `+toolchain`
/// skipped) with the words after it.
fn cargo(part: &Part) -> Option<(&str, &[Word])> {
    let run = part
        .iter()
        .filter_map(|simple| run(simple))
        .find(|run| run.name == "cargo")?;
    let at = run
        .args
        .iter()
        .position(|word| !word.text.starts_with('+'))?;
    Some((run.args[at].text.as_str(), &run.args[at + 1..]))
}

/// The first rule a part matches: e2e > llvm-cov > test > build/clippy >
/// fmt > wait > dagq > git > other. Only the words in command position
/// count as what it runs.
fn rank(part: &Part) -> &'static str {
    let cargo = cargo(part);
    let sub = cargo.map(|(sub, _)| sub);
    let e2e = cargo.is_some_and(|(_, args)| {
        args.windows(2)
            .any(|pair| pair[0].text == "--test" && pair[1].text == "e2e")
            || args.iter().any(|word| word.text == "--test=e2e")
    });
    let runs: Vec<Run> = part.iter().filter_map(|simple| run(simple)).collect();
    let runs_named = |name: &str| runs.iter().any(|run| run.name == name);
    if e2e {
        E2E
    } else if sub == Some("llvm-cov") {
        LLVM_COV
    } else if matches!(sub, Some("test" | "nextest")) {
        TEST
    } else if matches!(sub, Some("build" | "clippy" | "check" | "run")) {
        BUILD
    } else if sub == Some("fmt") {
        FMT
    } else if runs.iter().any(|run| {
        matches!(run.name, "sleep" | "uptime")
            || run.lead.contains(&"until")
            || run.name == "sysctl" && run.args.iter().any(|w| w.text.contains("loadavg"))
    }) {
        WAIT
    } else if runs_named("dagq") {
        DAGQ
    } else if runs_named("git") {
        GIT
    } else {
        OTHER_COMMAND
    }
}

/// Whether a shell command runs the whole test suite in one of its parts:
/// `cargo test` (or `cargo nextest run`) with only flags before `--`, none
/// of them choosing a target or filtering, except `--test it` (or
/// `--test=it`) as the only target: the one integration test binary runs
/// nearly every test the llvm-cov gate runs (ADR-0078). A filter word
/// (`--test it runtime_claim::`) or another target beside it makes the
/// part narrowed.
pub fn full_test(command: &str) -> bool {
    parts(command).iter().any(runs_full_test)
}

fn runs_full_test(part: &Part) -> bool {
    let Some((sub, args)) = cargo(part) else {
        return false;
    };
    let mut rest = args.iter().map(|word| word.text.as_str());
    match sub {
        "test" => {}
        "nextest" if rest.next() == Some("run") => {}
        _ => return false,
    }
    const TARGETED: [&str; 9] = [
        "--test",
        "--lib",
        "--bin",
        "--bins",
        "--doc",
        "--example",
        "--package",
        "-p",
        "-E",
    ];
    let mut words = Vec::new();
    let mut rest = rest.take_while(|word| *word != "--");
    while let Some(word) = rest.next() {
        // A redirection (`2>&1`, `> out.txt`) is not an argument.
        let operator = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
        if operator.starts_with(['>', '<']) {
            if operator.trim_start_matches(['>', '<', '|']).is_empty() {
                rest.next();
            }
            continue;
        }
        words.push(word);
    }
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        if word == "--test" && words.get(index + 1) == Some(&"it") {
            index += 2;
            continue;
        }
        let whole = word == "--test=it";
        if !word.starts_with('-')
            || !whole
                && TARGETED
                    .iter()
                    .any(|t| word == *t || word.starts_with(&format!("{t}=")))
        {
            return false;
        }
        index += 1;
    }
    true
}

/// What a command runs that `integrate` also runs: the llvm-cov gate, the
/// whole test suite, the e2e test.
fn verification_classes(command: &str) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = parts(command)
        .iter()
        .filter_map(|part| match rank(part) {
            LLVM_COV => Some(LLVM_COV),
            E2E => Some(E2E),
            TEST if runs_full_test(part) => Some("full_test"),
            _ => None,
        })
        .collect();
    classes.sort_unstable();
    classes.dedup();
    classes
}

/// One tool call of the span, with its times (unix milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub tool_use_id: String,
    pub tool: String,
    pub category: &'static str,
    pub start: i64,
    pub end: i64,
    /// Its end was seen (a result, or the notification of a background
    /// one); otherwise it is cut at the span's end.
    pub finished: bool,
    pub background: bool,
    pub exit_code: Option<i64>,
    /// `None` when its outcome is unknown.
    pub failed: Option<bool>,
    pub status: Option<String>,
    /// The shell command, for the run directory's `worktime.jsonl` only.
    pub command: Option<String>,
    /// The tests its output names as failed (task 515), whatever its exit
    /// (`cargo test … | tail` exits 0): a foreground command that runs
    /// tests ([`TEST_KINDS`]) only, since a background command's output is
    /// not in the transcript, and another command's output (a file shown,
    /// a search) may quote the marks.
    pub failed_tests: Vec<String>,
}

impl Command {
    fn priority(&self) -> u8 {
        if self.category == SUBAGENT {
            3
        } else if self.background {
            2
        } else {
            1
        }
    }

    /// The line of `worktime.jsonl` of this command.
    pub fn line(&self, span: &Value) -> Value {
        const MAX: usize = 300;
        let command = self.command.as_deref().map(|command| {
            let mut end = command.len().min(MAX);
            while !command.is_char_boundary(end) {
                end -= 1;
            }
            &command[..end]
        });
        json!({
            "opened_event_id": span["opened_event_id"],
            "kind": span["kind"],
            "attempt": span["attempt"],
            "tool": self.tool,
            "category": self.category,
            "start": millis_text(self.start),
            "end": millis_text(self.end),
            "secs": secs(self.end - self.start),
            "finished": self.finished,
            "background": self.background,
            "exit_code": self.exit_code,
            "failed": self.failed,
            "status": self.status,
            "command": command,
            "failed_tests": self.failed_tests,
        })
    }
}

/// Whole seconds of `millis`, rounded.
fn secs(millis: i64) -> i64 {
    (millis + 500).div_euclid(1000)
}

/// The tool calls that start in the span `from`..`to` (unix
/// milliseconds), in the order they started. A call whose end was not
/// seen is cut at `to`.
pub fn commands(records: &[TranscriptRecord], from: i64, to: i64) -> Vec<Command> {
    let mut results = BTreeMap::new();
    let mut notices = BTreeMap::new();
    for record in records {
        for result in &record.tool_results {
            results
                .entry(result.tool_use_id.clone())
                .or_insert((record.at, result));
        }
        if let Some(notice) = &record.notification {
            notices
                .entry(notice.tool_use_id.clone())
                .or_insert((record.at, notice));
        }
    }
    let mut commands = Vec::new();
    for record in records.iter().filter(|r| !r.sidechain) {
        if record.at < from || record.at >= to {
            continue;
        }
        for tool_use in &record.tool_uses {
            let result = results.get(&tool_use.id).copied();
            let notice = notices.get(&tool_use.id).copied();
            let shell = tool_use.command.is_some();
            let (category, background, end) = if tool_use.name == "Agent" {
                (
                    SUBAGENT,
                    notice.is_some(),
                    notice.map(|(at, _)| at).or(result.map(|(at, _)| at)),
                )
            } else if shell && (tool_use.background || notice.is_some()) {
                // Run in the background from the start, or moved there
                // while it ran: it ends at its notice.
                (
                    classify(tool_use.command.as_deref().unwrap_or_default()),
                    true,
                    notice.map(|(at, _)| at),
                )
            } else if shell {
                (
                    classify(tool_use.command.as_deref().unwrap_or_default()),
                    false,
                    result.map(|(at, _)| at),
                )
            } else if WAIT_TOOLS.contains(&tool_use.name.as_str()) {
                (WAIT, false, result.map(|(at, _)| at))
            } else {
                (TOOL, false, result.map(|(at, _)| at))
            };
            let failed_tests = match (background, result) {
                (false, Some((_, result))) if shell && TEST_KINDS.contains(&category) => {
                    result.failed_tests.names.clone()
                }
                _ => Vec::new(),
            };
            let (exit_code, failed, status) = match (background, notice, result) {
                (true, Some((_, notice)), _) => (
                    notice.exit_code,
                    match notice.status.as_deref() {
                        Some("failed") => Some(true),
                        Some("completed") => Some(notice.exit_code.is_some_and(|code| code != 0)),
                        _ => None,
                    },
                    notice.status.clone(),
                ),
                (false, _, Some((_, result))) => (
                    result.exit_code,
                    Some(result.is_error || result.exit_code.is_some_and(|code| code != 0)),
                    None,
                ),
                _ => (None, None, None),
            };
            commands.push(Command {
                tool_use_id: tool_use.id.clone(),
                tool: tool_use.name.clone(),
                category,
                start: record.at,
                end: end.map_or(to, |end| end.clamp(record.at, to)),
                finished: end.is_some(),
                background,
                exit_code,
                failed,
                status,
                command: tool_use.command.clone(),
                failed_tests,
            });
        }
    }
    commands
}

/// The model's time: from a record of the main session to the `assistant`
/// record that follows it.
fn model_intervals(records: &[TranscriptRecord]) -> Vec<(i64, i64)> {
    let mut main: Vec<&TranscriptRecord> = records
        .iter()
        .filter(|r| !r.sidechain && r.role != RecordRole::Other)
        .collect();
    main.sort_by_key(|r| r.at);
    main.windows(2)
        .filter(|pair| pair[1].assistant)
        .map(|pair| (pair[0].at, pair[1].at))
        .collect()
}

/// The work of one span: seconds per category, the heavy commands'
/// runs and failures, and what it ran that `integrate` runs again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakdown {
    pub total_millis: i64,
    pub millis: BTreeMap<&'static str, i64>,
    pub commands: Vec<Command>,
    /// Commands that ran a check `integrate` runs too (one of the task's
    /// verification commands: the llvm-cov gate, the whole `cargo test`,
    /// the e2e test).
    pub verification_repeats: usize,
    /// Commands that ran the whole `cargo test`.
    pub full_tests: usize,
    /// Commands that ran llvm-cov.
    pub llvm_cov_runs: usize,
}

/// The breakdown of the span `from`..`to` (unix milliseconds) of
/// `records`, whose task verifies with `verification`.
pub fn breakdown(
    records: &[TranscriptRecord],
    from: i64,
    to: i64,
    verification: &[String],
) -> Breakdown {
    let to = to.max(from);
    let commands = commands(records, from, to);
    let order = |category: &str| {
        CATEGORIES
            .iter()
            .position(|c| *c == category)
            .unwrap_or(CATEGORIES.len())
    };
    // (start, end, priority, category), cut to the span.
    let mut intervals: Vec<(i64, i64, u8, &'static str)> = commands
        .iter()
        .map(|c| (c.start, c.end, c.priority(), c.category))
        .chain(
            model_intervals(records)
                .into_iter()
                .map(|(start, end)| (start, end, 4, MODEL)),
        )
        .map(|(start, end, priority, category)| (start.max(from), end.min(to), priority, category))
        .filter(|(start, end, ..)| start < end)
        .collect();
    intervals
        .sort_by_key(|&(start, end, priority, category)| (priority, order(category), start, end));
    let mut bounds: Vec<i64> = intervals
        .iter()
        .flat_map(|&(start, end, ..)| [start, end])
        .chain([from, to])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();
    let mut millis: BTreeMap<&'static str, i64> = BTreeMap::new();
    for pair in bounds.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let category = intervals
            .iter()
            .find(|&&(s, e, ..)| s <= start && end <= e)
            .map_or(IDLE, |&(.., category)| category);
        *millis.entry(category).or_default() += end - start;
    }
    let wanted: Vec<&str> = verification
        .iter()
        .flat_map(|command| verification_classes(command))
        .collect();
    let shell = || commands.iter().filter_map(|c| c.command.as_deref());
    Breakdown {
        total_millis: to - from,
        millis,
        verification_repeats: shell()
            .filter(|command| {
                verification_classes(command)
                    .iter()
                    .any(|class| wanted.contains(class))
            })
            .count(),
        full_tests: shell().filter(|command| full_test(command)).count(),
        llvm_cov_runs: shell()
            .filter(|command| parts(command).iter().any(|p| rank(p) == LLVM_COV))
            .count(),
        commands,
    }
}

impl Breakdown {
    /// The `work` of the span's `session_closed`: values only, no command
    /// and no path (the event rule of ADR A). `heavy` lists the heavy
    /// commands for `timeline`.
    pub fn payload(&self) -> Value {
        let secs_by: BTreeMap<&str, i64> = CATEGORIES
            .iter()
            .filter_map(|c| Some((*c, secs(*self.millis.get(c)?))))
            .filter(|(_, secs)| *secs > 0)
            .collect();
        let heavy: Vec<&Command> = self
            .commands
            .iter()
            .filter(|c| HEAVY.contains(&c.category))
            .collect();
        let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for command in &heavy {
            let count = counts.entry(command.category).or_default();
            count.0 += 1;
            count.1 += usize::from(command.failed == Some(true));
        }
        let mut payload = json!({
            "total_secs": secs(self.total_millis),
            "secs": secs_by,
            "commands": counts
                .into_iter()
                .map(|(category, (runs, failed))| (category, json!({"runs": runs, "failed": failed})))
                .collect::<BTreeMap<_, _>>(),
            "verification_repeats": self.verification_repeats,
            "full_tests": self.full_tests,
            "llvm_cov_runs": self.llvm_cov_runs,
            "heavy": heavy
                .iter()
                .map(|c| json!({
                    "category": c.category,
                    "start": millis_text(c.start),
                    "end": millis_text(c.end),
                    "secs": secs(c.end - c.start),
                    "background": c.background,
                    "finished": c.finished,
                    "failed": c.failed,
                }))
                .collect::<Vec<_>>(),
        });
        // The tests its commands named as failed, each once in the order
        // first named (task 515); left out when none, as before.
        let mut failed_tests: Vec<&String> = Vec::new();
        for name in self.commands.iter().flat_map(|c| &c.failed_tests) {
            if failed_tests.len() < MAX_SPAN_FAILED_TESTS && !failed_tests.contains(&name) {
                failed_tests.push(name);
            }
        }
        if !failed_tests.is_empty() {
            payload["failed_tests"] = json!(failed_tests);
        }
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript::Transcript;

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const BASE: i64 = 1_790_000_000_000;

    fn at(secs: i64) -> String {
        millis_text(BASE + secs * 1000)
    }

    fn ms(secs: i64) -> i64 {
        BASE + secs * 1000
    }

    fn line(kind: &str, secs: i64, extra: Value) -> String {
        let mut value = json!({
            "type": kind,
            "timestamp": at(secs),
            "sessionId": SESSION,
            "isSidechain": false,
            "version": "2.1.283",
        });
        for (key, field) in extra.as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    }

    fn input(secs: i64, text: &str) -> String {
        line("user", secs, json!({"message": {"content": text}}))
    }

    fn call(secs: i64, id: &str, name: &str, input: Value) -> String {
        line(
            "assistant",
            secs,
            json!({"message": {"content": [
                {"type": "text", "text": "..."},
                {"type": "tool_use", "id": id, "name": name, "input": input},
            ]}}),
        )
    }

    fn bash(secs: i64, id: &str, command: &str, background: bool) -> String {
        call(
            secs,
            id,
            "Bash",
            json!({"command": command, "run_in_background": background}),
        )
    }

    fn result(secs: i64, id: &str, text: &str, error: bool) -> String {
        line(
            "user",
            secs,
            json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": error},
            ]}}),
        )
    }

    fn notice(secs: i64, id: &str, status: &str, summary: &str) -> String {
        let content = format!(
            "<task-notification>\n<task-id>x</task-id>\n<tool-use-id>{id}</tool-use-id>\n<status>{status}</status>\n<summary>{summary}</summary>\n</task-notification>"
        );
        line(
            "queue-operation",
            secs,
            json!({"operation": "enqueue", "content": content}),
        )
    }

    fn records(lines: &[String]) -> Vec<TranscriptRecord> {
        Transcript::parse(&lines.join("\n"), SESSION)
            .unwrap()
            .records
    }

    #[test]
    fn commands_are_classified_by_the_heaviest_thing_they_run() {
        assert_eq!(
            classify("cargo test --locked --test e2e -- --ignored 2>&1 | tail"),
            E2E
        );
        assert_eq!(
            classify("cargo llvm-cov nextest --locked --fail-under-lines 80"),
            LLVM_COV
        );
        assert_eq!(classify("cargo +nightly test --lib stats"), TEST);
        assert_eq!(classify("cargo nextest run"), TEST);
        assert_eq!(
            classify("cargo clippy --locked --all-targets -- -D warnings"),
            BUILD
        );
        assert_eq!(classify("cargo fmt --all --check"), FMT);
        assert_eq!(classify("sleep 30"), WAIT);
        assert_eq!(classify("~/.local/bin/dagq ask --run r"), DAGQ);
        assert_eq!(classify("git status && git diff"), GIT);
        assert_eq!(classify("ls src"), OTHER_COMMAND);
        // Naming llvm-cov is not running it.
        assert_eq!(classify("grep -n llvm-cov AGENTS.md"), OTHER_COMMAND);
        assert_eq!(classify("git commit -m 'cargo llvm-cov'"), GIT);
        // Two heavy kinds joined make a chain; fmt is not heavy.
        assert_eq!(
            classify("cargo fmt --all && cargo clippy --all-targets && cargo test --locked"),
            CHAIN
        );
        assert_eq!(classify("cargo fmt --all\ncargo clippy"), BUILD);
        assert_eq!(classify("cargo build; cargo build --release"), BUILD);
    }

    #[test]
    fn existing_rules_hold_with_command_positions() {
        assert_eq!(classify("cargo fmt && cargo test"), TEST);
        assert_eq!(classify("cargo build && cargo test"), CHAIN);
        assert_eq!(
            classify("cd x && cargo test --locked --test it foo::"),
            TEST
        );
        assert_eq!(classify("cargo test 2>&1 | tail"), TEST);
        assert_eq!(classify("RUSTC_WRAPPER=sccache cargo build"), BUILD);
        assert_eq!(classify("FOO=\"a b\" cargo build"), BUILD);
        assert_eq!(classify("git commit -m \"cargo test\""), GIT);
        assert_eq!(classify("time cargo test --lib x"), TEST);
        assert_eq!(classify("env -i A=1 cargo clippy"), BUILD);
        assert_eq!(classify("timeout -s KILL 600 cargo test"), TEST);
        assert_eq!(classify("nohup cargo build >/dev/null 2>&1 &"), BUILD);
        assert_eq!(classify("echo start | cargo test"), TEST);
        assert_eq!(classify("X=$(cargo check --message-format json)"), BUILD);
        assert_eq!(classify("echo `git rev-parse HEAD`"), GIT);
        assert_eq!(classify("(cd x; cargo fmt)"), FMT);
        assert_eq!(classify("until [ -f done ]; do sleep 5; done"), WAIT);
        assert_eq!(classify("sysctl -n vm.loadavg"), WAIT);
        assert_eq!(classify("cargo build & cargo test"), CHAIN);
        assert_eq!(classify("echo a \\\n  && cargo test"), TEST);
        // Prefixes and their flags that take a value.
        assert_eq!(classify("nice -n 10 cargo test"), TEST);
        assert_eq!(classify("ls | xargs -n 1 cargo fmt"), FMT);
        assert_eq!(classify("env -u X cargo build"), BUILD);
        assert_eq!(classify("timeout --signal KILL 600 cargo test"), TEST);
        assert_eq!(classify(r"\cargo build"), BUILD);
        // A substitution in double quotes runs its commands.
        assert_eq!(
            classify("OUT=\"$(cargo test --locked 2>&1)\"; echo $OUT"),
            TEST
        );
        assert!(full_test("OUT=\"$(cargo test --locked 2>&1)\""));
        // An arithmetic `<<` is no heredoc.
        assert_eq!(classify("echo $((1<<2))\ncargo build"), BUILD);
    }

    #[test]
    fn quotes_heredocs_and_arguments_are_not_commands() {
        // A question quoting a command runs dagq only (manual-smoke, task 547).
        assert_eq!(
            classify(
                "dagq ask --run R --kind worker_question --because scope --topic acceptance_conflict --question \
                 'Step 5 requires `sleep 40 && cargo build --release`; ok?'"
            ),
            DAGQ
        );
        assert_eq!(classify("dagq note \"a && cargo test; cargo build\""), DAGQ);
        // A heredoc's body is text, not commands.
        let receipt = "cat > receipt.json.tmp <<'EOF'\n\
            {\"summary\": \"cargo build (bg, ok), cargo test (bg, ok) && more\"}\n\
            EOF\n";
        assert_eq!(classify(receipt), OTHER_COMMAND);
        assert_eq!(
            classify(&format!("{receipt}mv receipt.json.tmp receipt.json")),
            OTHER_COMMAND
        );
        assert_eq!(
            classify("cat <<-END > x\n\tcargo test\n\tEND\ncargo fmt"),
            FMT
        );
        assert_eq!(classify("cat <<\"EOF\"\ncargo build\nEOF"), OTHER_COMMAND);
        // A commit message in a heredoc in `"$(…)"`, with an odd quote.
        assert_eq!(
            classify(
                "git commit -m \"$(cat <<'EOF'\nfix: handle \"x\n\ncargo test passes\nEOF\n)\""
            ),
            GIT
        );
        // A here-string is not a heredoc.
        assert_eq!(classify("grep x <<< 'y'\ncargo fmt"), FMT);
        // A word that is not in command position does not run.
        assert_eq!(classify("echo cargo test"), OTHER_COMMAND);
        assert_eq!(classify("which cargo git dagq"), OTHER_COMMAND);
        assert_eq!(classify("'cargo' test"), OTHER_COMMAND);
        assert_eq!(classify("echo \"$(date) cargo test\""), OTHER_COMMAND);
        assert_eq!(classify("# cargo test\nls"), OTHER_COMMAND);
        assert_eq!(classify("echo 'sleep 5'"), OTHER_COMMAND);
        assert_eq!(classify(r"echo \; cargo test"), OTHER_COMMAND);
        // Quoted separators do not split.
        assert_eq!(parts("echo 'a && b; c || d' && ls").len(), 2);
        assert_eq!(parts("echo \"a\nb\"").len(), 1);
    }

    #[test]
    fn the_whole_suite_is_cargo_test_with_flags_only() {
        assert!(full_test("cargo test --locked"));
        assert!(full_test("cargo test --locked 2>&1 | tail -5"));
        assert!(full_test("cargo nextest run --workspace"));
        assert!(full_test("cargo test -- --ignored"));
        assert!(full_test("cargo test --locked --test it"));
        assert!(full_test("cargo test --locked --test=it 2>&1 | tail"));
        assert!(!full_test("cargo test --locked --test it runtime_claim::"));
        assert!(!full_test("cargo test --locked --test e2e -- --ignored"));
        assert!(!full_test("cargo test --locked --test plugin"));
        assert!(!full_test("cargo test --locked --test it --lib"));
        assert!(!full_test("cargo test --locked --test it cli_stats::"));
        assert!(!full_test("cargo test --lib domain::worktime"));
        assert!(!full_test("cargo test stats"));
        assert!(!full_test("cargo test -p=dagq"));
        assert!(!full_test("cargo build"));
        assert!(!full_test("ls"));
        assert!(full_test("cd x && cargo test --locked"));
        assert!(!full_test("git commit -m 'cargo test --locked'"));
        assert!(!full_test("cat <<EOF\ncargo test\nEOF"));
        assert!(!full_test("cargo test --locked > out.txt stats"));
    }

    /// A session of several turns with foreground and background commands,
    /// an async subagent, a chain, and a command cut by the span's end.
    #[test]
    fn a_span_is_split_by_priority_and_background_work_ends_at_its_notice() {
        let lines = [
            input(0, "do the task"),
            // 0..10 the model; 10..70 a foreground clippy.
            bash(10, "t1", "cargo clippy --all-targets", false),
            result(70, "t1", "ok", false),
            // 70..80 the model; a background llvm-cov 80..380.
            bash(
                80,
                "t2",
                "cargo llvm-cov --locked --fail-under-lines 80",
                true,
            ),
            result(81, "t2", "Command running in background", false),
            // 81..90 the model; an async subagent 90..200.
            call(90, "t3", "Agent", json!({"description": "review"})),
            result(91, "t3", "Async agent launched", false),
            // 91..95 the model; a failing foreground test 95..125.
            bash(95, "t4", "cargo test --locked --test it cli_stats::", false),
            result(125, "t4", "Exit code 101\nfailures", true),
            call(130, "t5", "Read", json!({"file_path": "x"})),
            result(131, "t5", "text", false),
            line(
                "assistant",
                135,
                json!({"message": {"content": [{"type": "text", "text": "waiting"}]}}),
            ),
            notice(200, "t3", "completed", "Agent \"review\" finished"),
            input(200, "<task-notification>...</task-notification>"),
            notice(
                380,
                "t2",
                "failed",
                "Background command \"cargo llvm-cov\" failed with exit code 1",
            ),
            input(380, "<task-notification>...</task-notification>"),
            // 380..390 the model; a chain 390..500; a full test cut at 600.
            bash(
                390,
                "t6",
                "cargo fmt --all --check && cargo clippy && cargo test --locked",
                false,
            ),
            result(500, "t6", "ok", false),
            bash(550, "t7", "cargo test --locked 2>&1 | tail", false),
        ];
        // A foreground build moved to the background ends at its notice.
        let moved = [
            input(0, "go"),
            bash(1, "m", "cargo build", false),
            result(2, "m", "Command was moved to the background", false),
            notice(
                40,
                "m",
                "completed",
                "Background command \"cargo build\" completed (exit code 0)",
            ),
        ];
        let moved = breakdown(&records(&moved), ms(0), ms(50), &[]);
        assert!(moved.commands[0].background);
        assert_eq!(moved.commands[0].end, ms(40));
        assert_eq!(moved.commands[0].failed, Some(false));
        let records = records(&lines);
        let verification = vec![
            "cargo fmt --all --check".to_owned(),
            "cargo llvm-cov --locked --fail-under-lines 80".to_owned(),
        ];
        let work = breakdown(&records, ms(0), ms(600), &verification);
        let secs_of = |category: &str| work.millis.get(category).copied().unwrap_or(0) / 1000;
        assert_eq!(work.total_millis, 600_000);
        assert_eq!(work.millis.values().sum::<i64>(), 600_000);
        assert_eq!(secs_of(BUILD), 60);
        assert_eq!(secs_of(TEST), 30 + 50);
        assert_eq!(secs_of(CHAIN), 110);
        assert_eq!(secs_of(TOOL), 1);
        // The background llvm-cov takes 80..380 but for the foreground
        // test and Read; the model and the subagent are below it.
        assert_eq!(secs_of(LLVM_COV), (380 - 80) - 30 - 1);
        // The subagent is under the background command: none of its own.
        assert_eq!(secs_of(SUBAGENT), 0);
        assert_eq!(secs_of(MODEL), 10 + 10 + 10 + 50);
        assert_eq!(secs_of(IDLE), 0);

        let command = |id: &str| work.commands.iter().find(|c| c.tool_use_id == id).unwrap();
        assert!(command("t2").background);
        assert_eq!(command("t2").exit_code, Some(1));
        assert_eq!(command("t2").failed, Some(true));
        assert_eq!(command("t2").end, ms(380));
        assert_eq!(command("t3").category, SUBAGENT);
        assert!(command("t3").background);
        assert_eq!(command("t3").end, ms(200));
        assert_eq!(command("t4").exit_code, Some(101));
        assert_eq!(command("t4").failed, Some(true));
        assert_eq!(command("t1").failed, Some(false));
        assert!(!command("t7").finished);
        assert_eq!(command("t7").end, ms(600));
        assert_eq!(command("t7").failed, None);
        // llvm-cov repeats integrate's gate; the full test suite is not in
        // this task's verification.
        assert_eq!(work.verification_repeats, 1);
        assert_eq!(work.full_tests, 2);
        assert_eq!(work.llvm_cov_runs, 1);

        let payload = work.payload();
        assert_eq!(payload["total_secs"], 600);
        assert_eq!(payload["secs"]["build"], 60);
        assert!(payload["secs"].get("idle").is_none());
        assert_eq!(payload["commands"]["test"], json!({"runs": 2, "failed": 1}));
        assert_eq!(
            payload["commands"]["llvm_cov"],
            json!({"runs": 1, "failed": 1})
        );
        assert_eq!(
            payload["commands"]["chain"],
            json!({"runs": 1, "failed": 0})
        );
        assert!(payload["commands"].get("fmt").is_none());
        let heavy = payload["heavy"].as_array().unwrap();
        assert_eq!(heavy.len(), 5);
        assert_eq!(heavy[1]["category"], "llvm_cov");
        assert_eq!(heavy[1]["background"], true);
        assert_eq!(heavy[1]["secs"], 300);
        // No command text or path in the event's payload.
        assert!(!payload.to_string().contains("cargo"));

        let span = json!({"opened_event_id": 3, "kind": "worker", "attempt": 1});
        let line = command("t2").line(&span);
        assert_eq!(line["kind"], "worker");
        assert_eq!(line["category"], "llvm_cov");
        assert_eq!(line["status"], "failed");
        assert_eq!(line["secs"], 300);
        assert!(
            line["command"]
                .as_str()
                .unwrap()
                .starts_with("cargo llvm-cov")
        );
    }

    /// A resume goes on in the same transcript: only the calls that start
    /// in its span are its own, and the time before its first record and
    /// after an unanswered input is idle.
    #[test]
    fn a_span_takes_only_its_own_part_of_the_transcript() {
        let lines = [
            input(0, "first"),
            bash(5, "a", "cargo test --locked", false),
            result(50, "a", "ok", false),
            // The resume, in the same session.
            input(1000, "resume"),
            bash(1010, "b", "git status", false),
            result(1011, "b", "clean", false),
            line(
                "assistant",
                1020,
                json!({"message": {"content": [{"type": "text", "text": "done"}]}}),
            ),
        ];
        let records = records(&lines);
        let resume = breakdown(&records, ms(990), ms(1100), &[]);
        assert_eq!(resume.commands.len(), 1);
        assert_eq!(resume.commands[0].category, GIT);
        assert_eq!(resume.full_tests, 0);
        assert_eq!(resume.millis[&GIT], 1000);
        assert_eq!(resume.millis[&MODEL], 10_000 + 9000);
        assert_eq!(resume.millis[&IDLE], 10_000 + 80_000);
        let worker = breakdown(&records, ms(0), ms(100), &["cargo test --locked".into()]);
        assert_eq!(worker.verification_repeats, 1);
        assert_eq!(worker.millis[&TEST], 45_000);
        // An empty span.
        let empty = breakdown(&records, ms(5000), ms(4000), &[]);
        assert_eq!(empty.total_millis, 0);
        assert!(empty.millis.is_empty());
    }

    #[test]
    fn a_long_command_is_cut_on_a_char_boundary_in_its_line() {
        let command = Command {
            tool_use_id: "x".into(),
            tool: "Bash".into(),
            category: OTHER_COMMAND,
            start: 0,
            end: 1500,
            finished: true,
            background: false,
            exit_code: None,
            failed: Some(false),
            status: None,
            command: Some("あ".repeat(200)),
            failed_tests: Vec::new(),
        };
        let line = command.line(&json!({}));
        assert_eq!(line["secs"], 2);
        assert_eq!(line["command"].as_str().unwrap().chars().count(), 100);
    }

    /// A failed foreground command's tests are kept on it, in its line and
    /// in the span's `work`; a passing one, a background one and a span
    /// without any add nothing (task 515).
    #[test]
    fn a_failed_commands_tests_are_named() {
        let failed = "Exit code 101\ntest a::breaks ... FAILED\ntest b::too ... FAILED\n\ntest result: FAILED. 0 passed; 2 failed";
        let lines = [
            input(0, "go"),
            bash(1, "t1", "cargo test --locked --lib a", false),
            result(5, "t1", failed, true),
            bash(6, "t2", "cargo test --locked --lib a", false),
            result(9, "t2", "test a::breaks ... ok", false),
            bash(10, "t3", "cargo test --locked --test it b::", true),
            result(11, "t3", "Command running in background with ID: x", false),
            notice(
                20,
                "t3",
                "failed",
                "Background command failed with exit code 101",
            ),
            // Piped to tail it exits 0, and its output still says.
            bash(21, "t4", "cargo test --locked --lib 2>&1 | tail -5", false),
            result(
                24,
                "t4",
                "test b::too ... FAILED\ntest c::piped ... FAILED",
                false,
            ),
            // A file shown is not a test run, whatever it quotes.
            bash(25, "t5", "sed -n 1,9p src/domain/worktime.rs", false),
            result(26, "t5", "test z::quoted ... FAILED", false),
        ];
        let records = records(&lines);
        let work = breakdown(&records, ms(0), ms(30), &[]);
        let names: Vec<&[String]> = work
            .commands
            .iter()
            .map(|c| c.failed_tests.as_slice())
            .collect();
        assert_eq!(
            names,
            [
                &["a::breaks".to_owned(), "b::too".to_owned()][..],
                &[],
                &[],
                &["b::too".to_owned(), "c::piped".to_owned()],
                &[],
            ]
        );
        // Each once in the span.
        assert_eq!(
            work.payload()["failed_tests"],
            json!(["a::breaks", "b::too", "c::piped"])
        );
        let span = json!({"opened_event_id": 1, "kind": "worker", "attempt": 1});
        assert_eq!(
            work.commands[0].line(&span)["failed_tests"],
            json!(["a::breaks", "b::too"])
        );
        let quiet = breakdown(&records, ms(6), ms(21), &[]);
        assert!(quiet.payload().get("failed_tests").is_none());
    }
}
