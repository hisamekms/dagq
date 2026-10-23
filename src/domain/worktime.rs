//! The work of a run's own session (worker, resume, revise) broken down by
//! what it spent its time on (task 514): the model thinking and writing,
//! the commands it ran by kind, its subagents, its waits, and idle time.
//! Derived from the session's transcript over the span, by fixed rules;
//! reading the transcript is the infrastructure's, this module is pure.
//!
//! Every millisecond of the span gets one category, by priority:
//! a foreground tool > a background command > a subagent > the model >
//! idle. A shell command is labelled by the rules its repository works by
//! ([`label`], [`crate::domain::command_rules::work_rules`]): the group
//! of the rule that classifies it, or [`OTHER_COMMAND`] when none does; a
//! background command and an async subagent end at their
//! `<task-notification>`. The tool hooks (`PreToolUse` / `PostToolUse`)
//! are not used: they cannot see a background command's end.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::command_rules::{CHECK_CLASSES, CommandClassifier, FULL_TEST, RuleSource};
use super::transcript::{RecordRole, TranscriptRecord, millis_text};
use super::turn::TurnCommand;

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
/// Commands joined with `&&`, `||`, `;`, `&` or a newline that ran two or
/// more heavy kinds, in the breakdowns that recorded it: a command is now
/// labelled by the rule of its earliest-matched part ([`label`]), so no
/// new breakdown has it.
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

/// The most failed test names one span's `work` keeps (task 515).
pub const MAX_SPAN_FAILED_TESTS: usize = 50;

/// Tools that only wait (for a wakeup, a monitor, a background output).
const WAIT_TOOLS: [&str; 4] = ["ScheduleWakeup", "Monitor", "TaskOutput", "BashOutput"];

/// The category of a shell `command` by `rules`, and whether it is one of
/// the heavy commands: the group of the rule its earliest-matched part
/// matched, heavy when that rule is the project's or a profile's (a tool's
/// command, not a wait, dagq or git), and [`OTHER_COMMAND`], not heavy,
/// when no rule matches.
pub fn label(command: &str, rules: &CommandClassifier) -> (&'static str, bool) {
    match rules.classify_text(command) {
        Some(matched) => (matched.group, matched.source != RuleSource::Common),
        None => (OTHER_COMMAND, false),
    }
}

/// Whether a command, `heavy` or not, runs tests, so its output is read
/// for the failed tests: a heavy one, when `rules` name failed tests
/// (`[telemetry] failed_tests`).
fn reads_tests(rules: &CommandClassifier, heavy: bool) -> bool {
    heavy && rules.names_failed_tests()
}

/// What `command` runs of `integrate`'s checks, by `rules`: the classes of
/// its parts that are [`CHECK_CLASSES`], each once.
pub fn check_classes(command: &str, rules: &CommandClassifier) -> Vec<&'static str> {
    let mut classes: Vec<&'static str> = rules
        .part_classes(command)
        .into_iter()
        .filter(|class| CHECK_CLASSES.contains(class))
        .collect();
    classes.sort_unstable();
    classes.dedup();
    classes
}

/// Whether a breakdown by `rules` counts `integrate`'s checks: when one of
/// them gives a class of [`CHECK_CLASSES`].
fn counts_checks(rules: &CommandClassifier) -> bool {
    rules.rules().counts_checks()
}

/// What `command` runs that a [`Breakdown`] counts, by `rules`.
fn runs_of(command: &str, rules: &CommandClassifier) -> Runs {
    let classes = rules.part_classes(command);
    Runs {
        full_test: classes.contains(&FULL_TEST),
        llvm_cov: classes.contains(&super::command_rules::LLVM_COV),
        classes: check_classes(command, rules),
    }
}

/// One tool call of the span, with its times (unix milliseconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub tool_use_id: String,
    pub tool: String,
    pub category: &'static str,
    /// It is one of the heavy commands, counted and failed per category
    /// and listed in `timeline` ([`label`]).
    pub heavy: bool,
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
    /// (`cargo test … | tail` exits 0): a foreground heavy command only,
    /// when the rules name failed tests (`[telemetry] failed_tests`),
    /// since a background command's output is not in the transcript, and another command's output (a file shown,
    /// a search) may quote the marks.
    pub failed_tests: Vec<String>,
    /// How its times were found when they are not a transcript's: a
    /// headless turn's command ([`turn_commands`]). `None` for a
    /// transcript's, and then not in its line.
    pub time_source: Option<&'static str>,
}

/// The times of a headless turn's command were when the wrapper read its
/// start and its end.
pub const TIMES_READ: &str = "read";
/// Its end was not read (the turn ended or was stopped before it, or the
/// span closed while it ran): it ends with the turn.
pub const TIMES_TURN_END: &str = "turn_end";
/// Its start was not read, only its end: it starts there, 0 seconds long.
pub const TIMES_COMPLETED_ONLY: &str = "completed_only";

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
        let mut line = json!({
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
        });
        if let Some(source) = self.time_source {
            line["time_source"] = json!(source);
        }
        line
    }
}

/// Whole seconds of `millis`, rounded.
fn secs(millis: i64) -> i64 {
    (millis + 500).div_euclid(1000)
}

/// The tool calls that start in the span `from`..`to` (unix
/// milliseconds), in the order they started. A call whose end was not
/// seen is cut at `to`. `rules` label the shell commands ([`label`]).
pub fn commands(
    records: &[TranscriptRecord],
    from: i64,
    to: i64,
    rules: &CommandClassifier,
) -> Vec<Command> {
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
            let shell_label = || label(tool_use.command.as_deref().unwrap_or_default(), rules);
            let ((category, heavy), background, end) = if tool_use.name == "Agent" {
                (
                    (SUBAGENT, false),
                    notice.is_some(),
                    notice.map(|(at, _)| at).or(result.map(|(at, _)| at)),
                )
            } else if shell && (tool_use.background || notice.is_some()) {
                // Run in the background from the start, or moved there
                // while it ran: it ends at its notice.
                (shell_label(), true, notice.map(|(at, _)| at))
            } else if shell {
                (shell_label(), false, result.map(|(at, _)| at))
            } else if WAIT_TOOLS.contains(&tool_use.name.as_str()) {
                ((WAIT, false), false, result.map(|(at, _)| at))
            } else {
                ((TOOL, false), false, result.map(|(at, _)| at))
            };
            let failed_tests = match (background, result) {
                (false, Some((_, result))) if shell && reads_tests(rules, heavy) => {
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
                heavy,
                start: record.at,
                end: end.map_or(to, |end| end.clamp(record.at, to)),
                finished: end.is_some(),
                background,
                exit_code,
                failed,
                status,
                command: tool_use.command.clone(),
                failed_tests,
                time_source: None,
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
    /// the e2e test). This and the next two are counted only by rules that
    /// give a class of [`CHECK_CLASSES`] ([`counts_checks`]), `None`
    /// otherwise.
    pub verification_repeats: Option<usize>,
    /// Commands that ran the whole `cargo test`.
    pub full_tests: Option<usize>,
    /// Commands that ran llvm-cov.
    pub llvm_cov_runs: Option<usize>,
    /// The end of the span it was made to (unix milliseconds).
    pub to: i64,
    /// The span cut where any interval starts or ends, each piece with
    /// the category it counts to, in order ([`Breakdown::retarget`]).
    pub windows: Vec<(i64, i64, &'static str)>,
    /// What each command runs that is counted, by rules that count the
    /// checks only: one per command, a tool's empty ([`Breakdown::verify`]).
    pub runs: Option<Vec<Runs>>,
    /// The classes of `integrate`'s checks the task's verification runs,
    /// those `verification_repeats` counts.
    pub wanted: Vec<&'static str>,
    /// The rules it labels and counts by.
    pub rules: CommandClassifier,
}

/// What one command runs that a [`Breakdown`] counts, by its rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runs {
    /// What it runs of `integrate`'s checks.
    pub classes: Vec<&'static str>,
    /// It runs the whole `cargo test`.
    pub full_test: bool,
    /// It runs llvm-cov.
    pub llvm_cov: bool,
}

/// The breakdown of the span `from`..`to` (unix milliseconds) of
/// `records`, whose task verifies with `verification`. `rules`
/// ([`crate::domain::command_rules::work_rules`]) label the commands
/// ([`label`]) and say whether what they repeat of `integrate`'s checks is
/// counted ([`counts_checks`]).
pub fn breakdown(
    records: &[TranscriptRecord],
    from: i64,
    to: i64,
    verification: &[String],
    rules: &CommandClassifier,
) -> Breakdown {
    #[cfg(test)]
    super::transcript::count_analysis();
    let to = to.max(from);
    let commands = commands(records, from, to, rules);
    assemble(
        commands,
        model_intervals(records),
        (from, to),
        verification,
        rules,
    )
}

/// A headless turn of a span (ADR-t813-2 decision 7): when it started and
/// ended (unix milliseconds; a turn that still runs ends with the span,
/// one begun before the span starts before it, `i64::MIN` when that is
/// not known) and the commands the wrapper read of it.
#[derive(Debug, Clone, Copy)]
pub struct HeadlessTurn<'a> {
    pub start: i64,
    pub end: i64,
    pub commands: &'a [TurnCommand],
}

/// The commands of the headless `turns` that start in the span
/// `from`..`to` (unix milliseconds), in the order they started, as
/// [`commands`] makes a transcript's. Each starts when the wrapper read
/// its start and ends when it read its end, both kept within its turn
/// ([`TIMES_READ`]); one whose end was not read ends with its turn,
/// unfinished ([`TIMES_TURN_END`]); one whose start was not read starts
/// at its end ([`TIMES_COMPLETED_ONLY`]); one of which neither was read
/// is left out. A `command_execution` is labelled by `rules` ([`label`]),
/// any other item is a [`TOOL`]. None runs in the background.
pub fn turn_commands(
    turns: &[HeadlessTurn<'_>],
    from: i64,
    to: i64,
    rules: &CommandClassifier,
) -> Vec<Command> {
    let mut commands = Vec::new();
    for turn in turns {
        let end_of_turn = turn.end.max(turn.start);
        let within = |at: i64| at.clamp(turn.start, end_of_turn);
        for item in turn.commands {
            let (start, end, source) = match (item.started.map(within), item.ended.map(within)) {
                (Some(start), Some(end)) => (start, Some(end.max(start)), TIMES_READ),
                (Some(start), None) => (start, None, TIMES_TURN_END),
                (None, Some(end)) => (end, Some(end), TIMES_COMPLETED_ONLY),
                (None, None) => continue,
            };
            if start < from || start >= to {
                continue;
            }
            let (category, heavy) = if item.tool == SHELL_ITEM {
                label(item.command.as_deref().unwrap_or_default(), rules)
            } else {
                (TOOL, false)
            };
            let failed = end.map(|_| {
                item.exit_code.is_some_and(|code| code != 0)
                    || item.status.as_deref() == Some("failed")
            });
            commands.push(Command {
                tool_use_id: item.id.clone(),
                tool: item.tool.clone(),
                category,
                heavy,
                start,
                end: end.unwrap_or(end_of_turn).clamp(start, to),
                finished: end.is_some(),
                background: false,
                exit_code: item.exit_code,
                failed,
                status: None,
                command: item.command.clone(),
                failed_tests: if reads_tests(rules, heavy) {
                    item.failed_tests.clone()
                } else {
                    Vec::new()
                },
                time_source: Some(source),
            });
        }
    }
    commands.sort_by_key(|command| command.start);
    commands
}

/// The item type of a headless turn's shell command (Codex's).
pub const SHELL_ITEM: &str = "command_execution";

/// The breakdown of the span `from`..`to` (unix milliseconds) of a
/// headless session from its `turns` (Codex's, which has no transcript),
/// as [`breakdown`] makes a transcript's: the commands are
/// [`turn_commands`], and the model's time is its turns' (a turn's time
/// that no command takes is the model's; between turns it is idle).
pub fn headless_breakdown(
    turns: &[HeadlessTurn<'_>],
    from: i64,
    to: i64,
    verification: &[String],
    rules: &CommandClassifier,
) -> Breakdown {
    let to = to.max(from);
    assemble(
        turn_commands(turns, from, to, rules),
        turns.iter().map(|turn| (turn.start, turn.end)).collect(),
        (from, to),
        verification,
        rules,
    )
}

/// The breakdown of the span `from`..`to` of `commands` and the model's
/// `model` intervals.
fn assemble(
    commands: Vec<Command>,
    model: Vec<(i64, i64)>,
    (from, to): (i64, i64),
    verification: &[String],
    rules: &CommandClassifier,
) -> Breakdown {
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
        .chain(model.into_iter().map(|(start, end)| (start, end, 4, MODEL)))
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
    let windows: Vec<(i64, i64, &'static str)> = bounds
        .windows(2)
        .map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            let category = intervals
                .iter()
                .find(|&&(s, e, ..)| s <= start && end <= e)
                .map_or(IDLE, |&(.., category)| category);
            (start, end, category)
        })
        .collect();
    let runs: Option<Vec<Runs>> = counts_checks(rules).then(|| {
        commands
            .iter()
            .map(|c| match c.command.as_deref() {
                Some(command) => runs_of(command, rules),
                None => Runs::default(),
            })
            .collect()
    });
    let mut breakdown = Breakdown {
        total_millis: to - from,
        millis: BTreeMap::new(),
        verification_repeats: None,
        full_tests: None,
        llvm_cov_runs: None,
        commands,
        to,
        windows,
        runs,
        wanted: Vec::new(),
        rules: rules.clone(),
    };
    breakdown.count();
    breakdown.verify(verification);
    breakdown
}

impl Breakdown {
    /// Count again what its commands repeat of `integrate`'s checks, for
    /// the task's `verification` (task 1334: the verification is read
    /// under the write lock, and the commands' classes were found before
    /// it). Nothing unless its rules count the checks.
    pub fn verify(&mut self, verification: &[String]) {
        self.wanted = verification
            .iter()
            .flat_map(|command| check_classes(command, &self.rules))
            .collect();
        self.count_repeats();
    }

    fn count_repeats(&mut self) {
        self.verification_repeats = self.runs.as_ref().map(|runs| {
            runs.iter()
                .filter(|runs| runs.classes.iter().any(|class| self.wanted.contains(class)))
                .count()
        });
    }

    /// Count its seconds by category from its windows, and what its
    /// commands run.
    fn count(&mut self) {
        self.millis = BTreeMap::new();
        for &(start, end, category) in &self.windows {
            *self.millis.entry(category).or_default() += end - start;
        }
        self.full_tests = self
            .runs
            .as_ref()
            .map(|runs| runs.iter().filter(|runs| runs.full_test).count());
        self.llvm_cov_runs = self
            .runs
            .as_ref()
            .map(|runs| runs.iter().filter(|runs| runs.llvm_cov).count());
        self.count_repeats();
    }

    /// Make it the breakdown of the same span to `to` instead, as
    /// [`breakdown`] of the same records would (task 1334), with nothing
    /// of the records read again. To an earlier end it is cut there,
    /// whatever the records: the commands that start before it are the
    /// same, each ends at the earlier of its end and `to`, and each window
    /// before it counts to the same category. To a later end it is longer
    /// only when every record (the last at `last`) is before its end: the
    /// commands whose end was not seen end at `to`, and only its last
    /// window, then not ended by any record, is longer. Returns whether it
    /// did; otherwise it is left as it was.
    pub fn retarget(&mut self, last: Option<i64>, to: i64) -> bool {
        let from = self.to - self.total_millis;
        let to = to.max(from);
        if to == self.to {
            return true;
        }
        if to < self.to {
            if let Some(runs) = self.runs.as_mut() {
                let mut kept = self.commands.iter().map(|command| command.start < to);
                runs.retain(|_| kept.next().unwrap_or(false));
            }
            self.commands.retain(|command| command.start < to);
            for command in &mut self.commands {
                command.end = command.end.min(to);
            }
            self.windows.retain(|&(start, ..)| start < to);
            if let Some(window) = self.windows.last_mut() {
                window.1 = window.1.min(to);
            }
        } else {
            if last.is_some_and(|last| last >= self.to) {
                return false;
            }
            for command in self.commands.iter_mut().filter(|c| !c.finished) {
                command.end = to;
            }
            match self.windows.last_mut() {
                Some(window) => window.1 = to,
                None => self.windows.push((from, to, IDLE)),
            }
        }
        self.total_millis = to - from;
        self.to = to;
        self.count();
        true
    }

    /// The `work` of the span's `session_closed`: values only, no command
    /// and no path (the event rule of ADR A). `heavy` lists the heavy
    /// commands for `timeline`.
    pub fn payload(&self) -> Value {
        let secs_by: BTreeMap<&str, i64> = self
            .millis
            .iter()
            .map(|(category, millis)| (*category, secs(*millis)))
            .filter(|(_, secs)| *secs > 0)
            .collect();
        let heavy: Vec<&Command> = self.commands.iter().filter(|c| c.heavy).collect();
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
        // The counts of `integrate`'s checks, left out by rules that do not
        // count them ([`counts_checks`]).
        for (key, count) in [
            ("verification_repeats", self.verification_repeats),
            ("full_tests", self.full_tests),
            ("llvm_cov_runs", self.llvm_cov_runs),
        ] {
            if let Some(count) = count {
                payload[key] = json!(count);
            }
        }
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

    /// The rules of this repository's `dagq.toml`: the Rust profile
    /// declared, with the project's rules for its test targets over it.
    fn cargo_rules() -> CommandClassifier {
        cargo_rules_with(false)
    }

    /// The same, naming the failed tests (`[telemetry] failed_tests`) or
    /// not.
    fn cargo_rules_with(failed_tests: bool) -> CommandClassifier {
        crate::domain::command_rules::work_rules(
            &crate::domain::toolchain::Toolchains::this_repository().with_telemetry(
                crate::domain::command_rules::TelemetrySettings {
                    commands: crate::domain::command_rules::project_rules(),
                    failed_tests,
                    ..Default::default()
                },
            ),
        )
    }

    /// The rules of a repository that declares no profile and no project
    /// rule: the common rules alone.
    fn no_rules() -> CommandClassifier {
        crate::domain::command_rules::work_rules(&crate::domain::toolchain::Toolchains::none())
    }

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
        Transcript::parse(
            &lines.join("\n"),
            SESSION,
            &crate::domain::toolchain::Toolchains::this_repository(),
        )
        .unwrap()
        .records
    }

    /// The same, read for the failed tests (`[telemetry] failed_tests` on).
    fn records_naming_failed_tests(lines: &[String]) -> Vec<TranscriptRecord> {
        let asks = crate::domain::command_rules::TelemetrySettings {
            failed_tests: true,
            ..Default::default()
        };
        Transcript::parse(
            &lines.join("\n"),
            SESSION,
            &crate::domain::toolchain::Toolchains::this_repository().with_telemetry(asks),
        )
        .unwrap()
        .records
    }

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
            // 380..390 the model; a line of three 390..500, a test by the
            // earliest rule its parts match; a full test cut at 600.
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
        let moved = breakdown(&records(&moved), ms(0), ms(50), &[], &cargo_rules());
        assert!(moved.commands[0].background);
        assert_eq!(moved.commands[0].end, ms(40));
        assert_eq!(moved.commands[0].failed, Some(false));
        let records = records(&lines);
        let verification = vec![
            "cargo fmt --all --check".to_owned(),
            "cargo llvm-cov --locked --fail-under-lines 80".to_owned(),
        ];
        let work = breakdown(&records, ms(0), ms(600), &verification, &cargo_rules());
        let secs_of = |category: &str| work.millis.get(category).copied().unwrap_or(0) / 1000;
        assert_eq!(work.total_millis, 600_000);
        assert_eq!(work.millis.values().sum::<i64>(), 600_000);
        assert_eq!(secs_of(BUILD), 60);
        assert_eq!(secs_of(TEST), 30 + 110 + 50);
        assert_eq!(secs_of(CHAIN), 0);
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
        assert_eq!(work.verification_repeats, Some(1));
        assert_eq!(work.full_tests, Some(2));
        assert_eq!(work.llvm_cov_runs, Some(1));

        let payload = work.payload();
        assert_eq!(payload["total_secs"], 600);
        assert_eq!(payload["secs"]["build"], 60);
        assert!(payload["secs"].get("idle").is_none());
        assert_eq!(payload["commands"]["test"], json!({"runs": 3, "failed": 1}));
        assert_eq!(
            payload["commands"]["llvm_cov"],
            json!({"runs": 1, "failed": 1})
        );
        assert!(payload["commands"].get("chain").is_none());
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
        let resume = breakdown(&records, ms(990), ms(1100), &[], &cargo_rules());
        assert_eq!(resume.commands.len(), 1);
        assert_eq!(resume.commands[0].category, GIT);
        assert_eq!(resume.full_tests, Some(0));
        assert_eq!(resume.millis[&GIT], 1000);
        assert_eq!(resume.millis[&MODEL], 10_000 + 9000);
        assert_eq!(resume.millis[&IDLE], 10_000 + 80_000);
        let worker = breakdown(
            &records,
            ms(0),
            ms(100),
            &["cargo test --locked".into()],
            &cargo_rules(),
        );
        assert_eq!(worker.verification_repeats, Some(1));
        assert_eq!(worker.millis[&TEST], 45_000);
        // An empty span.
        let empty = breakdown(&records, ms(5000), ms(4000), &[], &cargo_rules());
        assert_eq!(empty.total_millis, 0);
        assert!(empty.millis.is_empty());
    }

    #[test]
    fn a_long_command_is_cut_on_a_char_boundary_in_its_line() {
        let command = Command {
            tool_use_id: "x".into(),
            tool: "Bash".into(),
            category: OTHER_COMMAND,
            heavy: false,
            start: 0,
            end: 1500,
            finished: true,
            background: false,
            exit_code: None,
            failed: Some(false),
            status: None,
            command: Some("あ".repeat(200)),
            failed_tests: Vec::new(),
            time_source: None,
        };
        let line = command.line(&json!({}));
        assert_eq!(line["secs"], 2);
        assert_eq!(line["command"].as_str().unwrap().chars().count(), 100);
        assert_eq!(line.get("time_source"), None);
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
        let records = records_naming_failed_tests(&lines);
        let work = breakdown(&records, ms(0), ms(30), &[], &cargo_rules_with(true));
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
        let quiet = breakdown(&records, ms(6), ms(21), &[], &cargo_rules_with(true));
        assert!(quiet.payload().get("failed_tests").is_none());
    }

    /// With no profile declared and no project rule, dagq's source or not,
    /// no cargo command is classified (a build neither: no rule is applied
    /// by default, ADR-t2268-1 decision 3), none of their failed tests is read, and the counts of what
    /// `integrate` repeats are neither kept nor written.
    #[test]
    fn without_a_profile_or_a_project_rule_no_cargo_command_is_classified_or_counted() {
        let lines = [
            input(0, "go"),
            bash(1, "a", "cargo llvm-cov --locked", false),
            result(10, "a", "Exit code 1", true),
            bash(11, "b", "cargo test --locked", false),
            result(20, "b", "test a::breaks ... FAILED", true),
            bash(21, "c", "cargo test --test e2e -- --ignored", false),
            result(30, "c", "ok", false),
            bash(31, "d", "cargo build && cargo test", false),
            result(40, "d", "ok", false),
        ];
        let records = records(&lines);
        let verification = ["cargo llvm-cov --locked".to_owned()];
        let work = breakdown(&records, ms(0), ms(50), &verification, &no_rules());
        let categories: Vec<&str> = work.commands.iter().map(|c| c.category).collect();
        assert_eq!(
            categories,
            [OTHER_COMMAND, OTHER_COMMAND, OTHER_COMMAND, OTHER_COMMAND]
        );
        assert!(work.commands.iter().all(|c| c.failed_tests.is_empty()));
        assert_eq!(work.verification_repeats, None);
        assert_eq!(work.full_tests, None);
        assert_eq!(work.llvm_cov_runs, None);
        let payload = work.payload();
        for key in [
            "verification_repeats",
            "full_tests",
            "llvm_cov_runs",
            "failed_tests",
        ] {
            assert!(payload.get(key).is_none(), "{key}: {payload}");
        }
        assert_eq!(payload["secs"]["other_command"], 36);
        assert_eq!(payload["commands"], json!({}));
        assert_eq!(payload["heavy"], json!([]));
        // The same commands by this repository's rules: the line of two by
        // the earliest rule its parts match.
        let cargo = breakdown(&records, ms(0), ms(50), &verification, &cargo_rules());
        let categories: Vec<&str> = cargo.commands.iter().map(|c| c.category).collect();
        assert_eq!(categories, [LLVM_COV, TEST, E2E, TEST]);
        assert_eq!(cargo.verification_repeats, Some(1));
        assert_eq!(cargo.full_tests, Some(2));
        assert_eq!(cargo.llvm_cov_runs, Some(1));
    }

    /// (4) The work breakdown names a test command's failed tests only when
    /// `[telemetry] failed_tests` is on and a declared profile reads them,
    /// for a Claude transcript and a Codex turn alike; this repository's
    /// rules too, none by default.
    #[test]
    fn failed_tests_are_named_only_when_on_and_by_a_declared_profile() {
        use crate::domain::command_rules::{TelemetrySettings, work_rules};
        use crate::domain::toolchain::Toolchains;
        let on = TelemetrySettings {
            failed_tests: true,
            ..TelemetrySettings::default()
        };
        let rust = Toolchains::this_repository();
        let lines = [
            input(0, "go"),
            bash(1, "a", "cargo test --locked", false),
            result(10, "a", "test a::b ... FAILED\ntest c::d ... FAILED", true),
        ];
        let claude = |read: &Toolchains, toolchains: &Toolchains| {
            let records = Transcript::parse(&lines.join("\n"), SESSION, read)
                .unwrap()
                .records;
            let rules = work_rules(toolchains);
            breakdown(&records, ms(0), ms(20), &[], &rules).commands[0]
                .failed_tests
                .clone()
        };
        let with_on = rust.clone().with_telemetry(on.clone());
        // No profile declared: none named, a project rule classing the
        // test command or not.
        let none_on = Toolchains::none().with_telemetry(TelemetrySettings {
            commands: vec![
                crate::domain::command_rules::CommandRule::new("test", "cargo")
                    .subcommands(&["test"]),
            ],
            ..on.clone()
        });
        assert!(claude(&rust, &rust).is_empty());
        assert_eq!(claude(&with_on, &with_on), ["a::b", "c::d"]);
        assert!(claude(&none_on, &none_on).is_empty());
        // This repository's rules: none by default.
        let records = Transcript::parse(&lines.join("\n"), SESSION, &rust)
            .unwrap()
            .records;
        assert!(
            breakdown(&records, ms(0), ms(20), &[], &cargo_rules()).commands[0]
                .failed_tests
                .is_empty()
        );
        // A Codex turn's command, the names as a reader asked for them kept.
        let mut test = item("t", Some("cargo test --locked"), (Some(1), Some(10)));
        test.exit_code = Some(101);
        test.failed_tests = vec!["a::b".into()];
        let items = [test];
        let turns = [HeadlessTurn {
            start: ms(0),
            end: ms(20),
            commands: &items,
        }];
        let codex = |toolchains: &Toolchains| {
            turn_commands(&turns, ms(0), ms(30), &work_rules(toolchains))[0]
                .failed_tests
                .clone()
        };
        assert!(codex(&rust).is_empty());
        assert_eq!(codex(&with_on), ["a::b"]);
        assert!(codex(&none_on).is_empty());
        assert!(
            turn_commands(&turns, ms(0), ms(30), &cargo_rules())[0]
                .failed_tests
                .is_empty()
        );
    }

    /// A breakdown made to one end and moved to another is the breakdown
    /// made to that end (task 1334): cut to any earlier end, records after
    /// it or not (a command whose result comes later is cut there), and
    /// longer to a later end when every record is before the earlier one.
    /// Longer past a record at or after its end it refuses.
    #[test]
    fn a_breakdown_retargeted_is_the_breakdown_to_that_end() {
        let verification = vec!["cargo llvm-cov --locked --fail-under-lines 80".to_owned()];
        let finished = [
            input(0, "go"),
            bash(1, "a", "cargo test --locked", false),
            result(9, "a", "test result: ok", false),
            call(10, "s", "Agent", json!({"prompt": "review"})),
            result(20, "s", "done", false),
            line(
                "assistant",
                25,
                json!({"message": {"content": [{"type": "text"}]}}),
            ),
        ];
        let mut running = finished.to_vec();
        running.extend([
            bash(30, "b", "cargo build", true),
            bash(31, "c", "cargo llvm-cov --locked", false),
        ]);
        let mut later = running.clone();
        later.extend([
            result(60, "c", "ok", false),
            input(70, "more"),
            bash(72, "d", "cargo test --locked", false),
            result(80, "d", "ok", false),
        ]);
        for lines in [&finished[..], &running[..], &later[..]] {
            let records = records(lines);
            let last = records.iter().map(|r| r.at).max();
            let fresh = |to: i64| breakdown(&records, ms(0), to, &verification, &cargo_rules());
            for (from, to) in [
                (ms(90), ms(120)),
                (ms(120), ms(90)),
                (ms(90), ms(75)),
                (ms(90), ms(31)),
                (ms(90), ms(5)),
                (ms(90), ms(0)),
                (ms(90), -ms(10)),
                (ms(40), ms(41)),
            ] {
                let mut moved = fresh(from);
                let before = moved.clone();
                if to > from && last.is_some_and(|last| last >= from) {
                    assert!(!moved.retarget(last, to), "{from} to {to}");
                    assert_eq!(moved, before);
                    continue;
                }
                assert!(moved.retarget(last, to), "{from} to {to}");
                assert_eq!(moved, fresh(to), "{from} to {to}");
                assert_eq!(moved.payload(), fresh(to).payload());
            }
            // A span starting at its end, made longer.
            let mut empty = breakdown(&records, ms(90), ms(90), &verification, &cargo_rules());
            assert!(empty.retarget(last, ms(100)));
            assert_eq!(
                empty,
                breakdown(&records, ms(90), ms(100), &verification, &cargo_rules())
            );
            let mut same = fresh(ms(90));
            assert!(same.retarget(last, ms(90)));
            assert_eq!(same, fresh(ms(90)));
        }
    }

    /// The repeats of `integrate`'s checks counted again for another
    /// verification are those [`breakdown`] counts with it (task 1334);
    /// without rules that count the checks there are none.
    #[test]
    fn a_breakdown_counts_its_repeats_again_for_another_verification() {
        let records = records(&[
            input(0, "go"),
            bash(1, "a", "cargo test --locked", false),
            result(9, "a", "ok", false),
            bash(10, "b", "cargo llvm-cov nextest --locked", false),
            result(20, "b", "ok", false),
        ]);
        let llvm_cov = vec!["cargo llvm-cov nextest --locked --workspace".to_owned()];
        let test = vec!["cargo test --locked".to_owned()];
        for rules in [cargo_rules(), no_rules()] {
            let mut work = breakdown(&records, ms(0), ms(30), &llvm_cov, &rules);
            for verification in [&test, &llvm_cov, &Vec::new()] {
                work.verify(verification);
                assert_eq!(
                    work,
                    breakdown(&records, ms(0), ms(30), verification, &rules)
                );
            }
        }
    }

    fn item(id: &str, command: Option<&str>, times: (Option<i64>, Option<i64>)) -> TurnCommand {
        TurnCommand {
            id: id.into(),
            tool: if command.is_some() {
                SHELL_ITEM.into()
            } else {
                "mcp_tool_call".into()
            },
            command: command.map(str::to_owned),
            started: times.0.map(ms),
            ended: times.1.map(ms),
            exit_code: times.1.and(command).map(|_| 0),
            status: None,
            failed_tests: Vec::new(),
        }
    }

    /// A headless turn's commands (task 1354) take the times the wrapper
    /// read, kept within their turn: one whose end was not read ends with
    /// the turn unfinished, one whose start was not read starts at its end,
    /// one of which neither was read is left out, and one that starts
    /// outside the span is not the span's. A shell command is classified as
    /// a transcript's; another item is a tool; the failed tests are kept
    /// for a test command only.
    #[test]
    fn a_headless_turns_commands_take_the_times_read_within_their_turn() {
        let mut test = item("t", Some("cargo test"), (Some(12), Some(30)));
        test.exit_code = Some(101);
        test.failed_tests = vec!["a::b".into()];
        let mut git = item("g", Some("git diff"), (Some(31), Some(32)));
        git.failed_tests = vec!["quoted::name".into()];
        let first = [
            // Read before the turn started (the clock): from its start.
            item("b", Some("cargo build"), (Some(5), Some(11))),
            test,
            git,
            item("x", Some("sleep 1"), (None, None)),
        ];
        let second = [
            item("o", Some("echo done"), (None, Some(55))),
            item("m", None, (Some(56), Some(58))),
            // Never ended: cut at the turn's end; read past the turn's end:
            // at it.
            item("r", Some("cargo build"), (Some(59), None)),
        ];
        let turns = [
            HeadlessTurn {
                start: ms(10),
                end: ms(40),
                commands: &first,
            },
            HeadlessTurn {
                start: ms(50),
                end: ms(60),
                commands: &second,
            },
        ];
        let commands = turn_commands(&turns, ms(0), ms(100), &cargo_rules_with(true));
        let got: Vec<_> = commands
            .iter()
            .map(|c| {
                (
                    c.tool_use_id.as_str(),
                    c.category,
                    (c.start - BASE) / 1000,
                    (c.end - BASE) / 1000,
                    c.finished,
                    c.time_source,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("b", BUILD, 10, 11, true, Some(TIMES_READ)),
                ("t", TEST, 12, 30, true, Some(TIMES_READ)),
                ("g", GIT, 31, 32, true, Some(TIMES_READ)),
                ("o", OTHER_COMMAND, 55, 55, true, Some(TIMES_COMPLETED_ONLY)),
                ("m", TOOL, 56, 58, true, Some(TIMES_READ)),
                ("r", BUILD, 59, 60, false, Some(TIMES_TURN_END)),
            ]
        );
        assert_eq!(commands[1].failed, Some(true));
        assert_eq!(commands[1].failed_tests, ["a::b"]);
        assert!(commands[2].failed_tests.is_empty());
        assert_eq!(commands[0].failed, Some(false));
        assert_eq!(commands[5].failed, None);
        assert!(commands.iter().all(|c| !c.background && c.status.is_none()));
        // Without a profile or a project rule a test is another command.
        assert_eq!(
            turn_commands(&turns, ms(0), ms(100), &no_rules())[1].category,
            OTHER_COMMAND
        );
        // A span from 50 has the second turn's only; one to 57 cuts it.
        let later = turn_commands(&turns, ms(50), ms(57), &cargo_rules());
        let ids: Vec<(&str, i64)> = later
            .iter()
            .map(|c| (c.tool_use_id.as_str(), (c.end - BASE) / 1000))
            .collect();
        assert_eq!(ids, [("o", 55), ("m", 57)]);
        // A turn begun before the span (its start not known): only its
        // commands that start in the span are the span's.
        let begun = [HeadlessTurn {
            start: i64::MIN,
            ..turns[0]
        }];
        let kept: Vec<String> = turn_commands(&begun, ms(12), ms(100), &cargo_rules())
            .into_iter()
            .map(|c| c.tool_use_id)
            .collect();
        assert_eq!(kept, ["t", "g"]);
        // Its line says how its times were found; a transcript's does not.
        let line = commands[3].line(&json!({"kind": "worker"}));
        assert_eq!(line["time_source"], TIMES_COMPLETED_ONLY);
        assert_eq!(line["secs"], 0);
    }

    /// A headless span's breakdown: its commands' time by category, the
    /// rest of its turns the model's, the time between and around them
    /// idle; and the cargo-only counts as a transcript's.
    #[test]
    fn a_headless_breakdown_counts_its_turns_as_the_models_time() {
        let first = [item(
            "c",
            Some("cargo llvm-cov --locked"),
            (Some(15), Some(25)),
        )];
        let turns = [
            HeadlessTurn {
                start: ms(10),
                end: ms(30),
                commands: &first,
            },
            HeadlessTurn {
                start: ms(50),
                end: ms(60),
                commands: &[],
            },
        ];
        let verification = ["cargo llvm-cov --locked --fail-under-lines 80".to_owned()];
        let breakdown = headless_breakdown(&turns, ms(0), ms(100), &verification, &cargo_rules());
        let payload = breakdown.payload();
        assert_eq!(payload["total_secs"], 100);
        assert_eq!(
            payload["secs"],
            json!({"llvm_cov": 10, "model": 20, "idle": 70})
        );
        assert_eq!(payload["verification_repeats"], 1);
        assert_eq!(payload["llvm_cov_runs"], 1);
        assert_eq!(payload["heavy"][0]["category"], LLVM_COV);
        // Cut and moved as a transcript's.
        let mut cut = breakdown.clone();
        assert!(cut.retarget(None, ms(20)));
        assert_eq!(
            cut.payload()["secs"],
            json!({"llvm_cov": 5, "model": 5, "idle": 10})
        );
        let plain = headless_breakdown(&turns, ms(0), ms(100), &verification, &no_rules());
        assert_eq!(plain.payload().get("verification_repeats"), None);
    }
}
