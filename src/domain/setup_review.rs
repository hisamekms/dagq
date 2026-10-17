//! What `setup-review` (ADR-t2259-1 decision 7) lets its coding agent
//! change in the temporary place it generates the review's settings in,
//! and the checks of what it wrote before the person sees it: the files
//! are `dagq.toml` and those under `.dagq/review/` only; of `dagq.toml`,
//! the built-in agents' tables (`[review.subagents."builtin:<name>"]`) and
//! the programs' (`[review.programs.<name>]`) only, every other table as it
//! was; and each program script under `.dagq/review/programs/` takes its
//! root from the cwd's Git work tree, as a program review runs it from a
//! temporary place with its cwd in the run's tree.

use serde::{Deserialize, Serialize};

use super::DomainError;
use super::review_builtins::{PREFIX, RULES_DIR};
use super::review_programs::SECTION as PROGRAMS_SECTION;

/// The command that generates the review's settings, as the guidance of
/// `init` and `doctor` names it.
pub const SETUP_COMMAND: &str = "dagq setup-review";
/// The file of the repository's configuration.
pub const CONFIG_FILE: &str = "dagq.toml";
/// Where the project's program scripts go.
pub const PROGRAMS_DIR: &str = ".dagq/review/programs";
/// What a program script runs to find its root: the cwd's Git work tree.
pub const ROOT_FROM_CWD: &str = "git rev-parse --show-toplevel";
/// The table prefix of the review's agents.
const SUBAGENTS_PREFIX: &str = "review.subagents.";

string_enum!(ChangeStatus {
    Added => "added",
    Modified => "modified",
    Deleted => "deleted",
});

/// A file the job changed in its temporary place, against the commit the
/// place was made from: its repository-relative path and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub path: String,
    pub status: ChangeStatus,
}

/// Whether the job may change `path`: `dagq.toml`, or a file under
/// `.dagq/review/`, never one with a `.git` part (in any case), which would
/// make a Git directory in the checkout, nor one with an empty, `.` or
/// `..` part.
pub fn writable(path: &str) -> bool {
    let plain = path.split('/').all(|part| {
        !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
    });
    plain
        && (path == CONFIG_FILE
            || path
                .strip_prefix(RULES_DIR)
                .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1))
}

/// The changes outside what the job may change, as an error naming each.
pub fn check_paths(changes: &[Change]) -> Result<(), String> {
    let outside: Vec<&str> = changes
        .iter()
        .filter(|change| !writable(&change.path))
        .map(|change| change.path.as_str())
        .collect();
    if outside.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the generation changed files outside {CONFIG_FILE} and {RULES_DIR}/, so nothing \
             is applied: {}",
            outside.join(", ")
        ))
    }
}

/// Whether the table `name` (a header's name, as written between the
/// brackets) is one the job writes: a built-in agent's
/// `review.subagents."builtin:<name>"`, or `review.programs` and any
/// program's `review.programs.<name>`, a built-in's or the project's.
pub fn generated_table(name: &str) -> bool {
    if name == PROGRAMS_SECTION
        || name
            .strip_prefix(PROGRAMS_SECTION)
            .is_some_and(|rest| rest.starts_with('.'))
    {
        return true;
    }
    name.strip_prefix(SUBAGENTS_PREFIX)
        .is_some_and(|agent| agent.trim().trim_matches(['"', '\'']).starts_with(PREFIX))
}

/// The tables of a `dagq.toml`'s `text`, each its header's name (`""` for
/// the keys before the first header) and its lines, trimmed, without the
/// blank lines and the comments: what two files are compared by, so a
/// change of a comment or of the blank lines is none.
pub fn tables(text: &str) -> Vec<(String, Vec<String>)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut tables = vec![(String::new(), Vec::new())];
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = header
                .split_once('#')
                .map_or(header, |(before, _)| before)
                .trim()
                .trim_end_matches(']')
                .trim();
            tables.push((name.to_owned(), Vec::new()));
            continue;
        }
        if let Some((_, lines)) = tables.last_mut() {
            lines.push(line.to_owned());
        }
    }
    tables.retain(|(name, lines)| !(name.is_empty() && lines.is_empty()));
    tables
}

/// The tables of `text` the job may not change, in a stable order.
fn kept_tables(text: Option<&str>) -> Vec<(String, Vec<String>)> {
    let mut kept: Vec<_> = text
        .map(tables)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| !generated_table(name))
        .collect();
    kept.sort();
    kept
}

/// Refuse a `dagq.toml` the job changed from `before` to `after` (`None`
/// where there is no file) in any table other than those it writes
/// ([`generated_table`]): the error names each table that differs.
pub fn check_config(before: Option<&str>, after: Option<&str>) -> Result<(), String> {
    let before = kept_tables(before);
    let after = kept_tables(after);
    if before == after {
        return Ok(());
    }
    let mut changed: Vec<String> = before
        .iter()
        .filter(|table| !after.contains(table))
        .chain(after.iter().filter(|table| !before.contains(table)))
        .map(|(name, _)| {
            if name.is_empty() {
                "the keys before the first table".to_owned()
            } else {
                format!("[{name}]")
            }
        })
        .collect();
    changed.dedup();
    Err(format!(
        "the generation changed {CONFIG_FILE} outside the built-in agents' and the programs' \
         tables, so nothing is applied: {}",
        changed.join(", ")
    ))
}

/// `diff` as the person is shown it: each control character but the line
/// break and the tab (an escape sequence's ESC and CSI, a carriage return
/// that would write over a line, a backspace) and each mark that turns the
/// direction of the text (the bidirectional overrides and isolates) is
/// written out as its escape (`\r`, `\x1b`, `\u{202e}`), so what the job
/// wrote cannot hide or redraw a line of the diff on the terminal.
pub fn visible(diff: &str) -> String {
    let mut shown = String::with_capacity(diff.len());
    for c in diff.chars() {
        match c {
            '\n' | '\t' => shown.push(c),
            '\r' => shown.push_str("\\r"),
            c if c.is_control() && u32::from(c) < 0x100 => {
                shown.push_str(&format!("\\x{:02x}", u32::from(c)));
            }
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                shown.push_str(&format!("\\u{{{:04x}}}", u32::from(c)));
            }
            c => shown.push(c),
        }
    }
    shown
}

/// The most cells of the table [`file_diff`] compares two files' lines
/// with; past it the whole of the old file is shown removed and the whole
/// of the new one added.
const DIFF_CELLS: usize = 4_000_000;
/// The lines of context around each change of [`file_diff`].
const DIFF_CONTEXT: usize = 3;

/// One step of turning the old lines into the new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Same,
    Removed,
    Added,
}

/// The steps from `old` to `new`: a longest common subsequence of the
/// lines between their common start and end, or, past [`DIFF_CELLS`], all
/// the old lines removed and all the new ones added.
fn steps(old: &[&str], new: &[&str]) -> Vec<Step> {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[start..old.len() - end], &new[start..new.len() - end]);
    let mut middle = Vec::new();
    if a.len().saturating_mul(b.len()) > DIFF_CELLS {
        middle.extend(std::iter::repeat_n(Step::Removed, a.len()));
        middle.extend(std::iter::repeat_n(Step::Added, b.len()));
    } else {
        let width = b.len() + 1;
        let mut table = vec![0u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                table[i * width + j] = if a[i] == b[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                middle.push(Step::Same);
                (i, j) = (i + 1, j + 1);
            } else if j < b.len()
                && (i == a.len() || table[i * width + j + 1] > table[(i + 1) * width + j])
            {
                middle.push(Step::Added);
                j += 1;
            } else {
                middle.push(Step::Removed);
                i += 1;
            }
        }
    }
    let mut all = vec![Step::Same; start];
    all.extend(middle);
    all.extend(std::iter::repeat_n(Step::Same, end));
    all
}

/// One line of a hunk, marked, with the note Git writes after a last line
/// without a line break.
fn hunk_line(out: &mut String, mark: char, line: &str) {
    out.push(mark);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}

/// The diff of one file the job changed, from the bytes as they are on
/// either side: `old` and `new` are the content and whether it is
/// executable, `None` where there is no file. No filter, conversion or
/// external diff of a repository's settings comes into it, so the person
/// sees exactly what is written. A side that is not UTF-8 is shown as a
/// binary change with its size.
pub fn file_diff(path: &str, old: Option<(&[u8], bool)>, new: Option<(&[u8], bool)>) -> String {
    let mode = |executable: bool| if executable { "100755" } else { "100644" };
    let mut out = format!("diff a/{path} b/{path}\n");
    match (old, new) {
        (None, Some((_, executable))) => {
            out.push_str(&format!("new file mode {}\n", mode(executable)));
        }
        (Some((_, executable)), None) => {
            out.push_str(&format!("deleted file mode {}\n", mode(executable)));
        }
        (Some((_, was)), Some((_, is))) if was != is => {
            out.push_str(&format!("old mode {}\nnew mode {}\n", mode(was), mode(is)));
        }
        _ => {}
    }
    fn text(side: Option<(&[u8], bool)>) -> Option<&str> {
        match side {
            None => Some(""),
            Some((bytes, _)) => std::str::from_utf8(bytes).ok(),
        }
    }
    let (Some(old_text), Some(new_text)) = (text(old), text(new)) else {
        let size = |side: Option<(&[u8], bool)>| side.map_or(0, |(bytes, _)| bytes.len());
        out.push_str(&format!(
            "Binary files a/{path} and b/{path} differ ({} bytes, {} bytes)\n",
            size(old),
            size(new)
        ));
        return out;
    };
    if old_text == new_text {
        return out;
    }
    out.push_str(&format!(
        "--- {}\n+++ {}\n",
        if old.is_some() {
            format!("a/{path}")
        } else {
            "/dev/null".to_owned()
        },
        if new.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".to_owned()
        },
    ));
    let a: Vec<&str> = old_text.split_inclusive('\n').collect();
    let b: Vec<&str> = new_text.split_inclusive('\n').collect();
    let steps = steps(&a, &b);
    // The old and the new line before each step.
    let mut at = Vec::with_capacity(steps.len() + 1);
    let (mut i, mut j) = (0, 0);
    for step in &steps {
        at.push((i, j));
        match step {
            Step::Same => (i, j) = (i + 1, j + 1),
            Step::Removed => i += 1,
            Step::Added => j += 1,
        }
    }
    at.push((i, j));
    let changed: Vec<usize> = (0..steps.len())
        .filter(|&k| steps[k] != Step::Same)
        .collect();
    let mut k = 0;
    while k < changed.len() {
        let from = changed[k].saturating_sub(DIFF_CONTEXT);
        let mut to = (changed[k] + 1 + DIFF_CONTEXT).min(steps.len());
        k += 1;
        while k < changed.len() && changed[k].saturating_sub(DIFF_CONTEXT) <= to {
            to = (changed[k] + 1 + DIFF_CONTEXT).min(steps.len());
            k += 1;
        }
        let olds = steps[from..to]
            .iter()
            .filter(|s| **s != Step::Added)
            .count();
        let news = steps[from..to]
            .iter()
            .filter(|s| **s != Step::Removed)
            .count();
        let start = |line: usize, count: usize| if count == 0 { line } else { line + 1 };
        out.push_str(&format!(
            "@@ -{},{olds} +{},{news} @@\n",
            start(at[from].0, olds),
            start(at[from].1, news)
        ));
        for (step, (i, j)) in steps[from..to].iter().zip(&at[from..to]) {
            match step {
                Step::Same => hunk_line(&mut out, ' ', a[*i]),
                Step::Removed => hunk_line(&mut out, '-', a[*i]),
                Step::Added => hunk_line(&mut out, '+', b[*j]),
            }
        }
    }
    out
}

/// Whether `path` is a program script of the project's: a `.sh` under
/// `.dagq/review/programs/`.
pub fn is_program_script(path: &str) -> bool {
    path.strip_prefix(PROGRAMS_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| name.ends_with(".sh"))
}

/// Whether a program script's `text` takes its root from the cwd's Git
/// work tree, not from where the script is: a program review copies the
/// landing branch's script to a temporary place and runs it with its cwd in
/// the run's tree. Some command of it at its top level
/// ([`top_level_commands`]: outside comments, quotes over any number of
/// lines and here-documents' bodies) must assign [`ROOT_FROM_CWD`]'s
/// output to a variable (`root=$(git rev-parse --show-toplevel)`, quoted
/// or not, after `export`, `local`, `readonly`, `declare` or `typeset` and
/// their options) or `cd` to it (`cd --` too), the command word for word
/// with no other option or argument than its errors sent to `/dev/null`,
/// and the value or the argument that output alone (quoted or not, nothing
/// joined to it);
/// the words
/// appearing anywhere else (a comment, a string, a here-document, another
/// command's argument) do not count. A fallback after it (`|| root=$(cd
/// "$(dirname "$0")/.." && pwd)`) is allowed.
pub fn takes_root_from_cwd(text: &str) -> bool {
    top_level_commands(text)
        .iter()
        .any(|command| takes_root(command))
}

/// What [`top_level_commands`] is inside of at a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nest {
    /// The script's top level, a subshell's or a group's `( )`: code,
    /// and a word starts after its `)`.
    Group,
    /// A command or process substitution's `$( )`, `<( )` or `>( )`:
    /// code, whose `)` ends a part of a word, not the word.
    Subst,
    /// An arithmetic `$(( ))` or `(( ))` and the parentheses in it: no
    /// comment and no here-document.
    Arith,
    /// A double-quoted string, which `$( )` and backquotes open into.
    Double,
    /// A backquoted command substitution.
    Back,
}

/// One level of [`top_level_commands`]: what it is inside of, and how many
/// `case`s are open in it, whose patterns end at a `)` that closes no
/// parenthesis.
#[derive(Debug, Clone, Copy)]
struct Level {
    nest: Nest,
    cases: usize,
}

/// A blank of the shell: what ends a word, as no other space does.
fn blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

/// Whether `word` starts at `at` in `chars` and ends there (the text's
/// end, a blank or an operator follows).
fn word_at(chars: &[char], at: usize, word: &str) -> bool {
    let mut end = at;
    for c in word.chars() {
        if chars.get(end) != Some(&c) {
            return false;
        }
        end += 1;
    }
    chars
        .get(end)
        .is_none_or(|&c| blank(c) || matches!(c, ';' | '&' | '|' | ')' | '<' | '>'))
}

/// Whether a reserved word (`case`, `esac`) would be one after `current`,
/// the text of the command so far: at the start of a command, after an
/// operator or a parenthesis that starts one, or after a word that does
/// (`then`, `do`, `else`, `if`, `while`, `until`, `{`, `!`).
fn command_start(current: &str) -> bool {
    let before = current.trim_end_matches(blank);
    before.is_empty()
        || before.ends_with([';', '&', '|', '('])
        || before.rsplit(blank).next().is_some_and(|word| {
            matches!(
                word,
                "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "{" | "!"
            )
        })
}

/// The commands of a shell script's `text` at its top level, without its
/// comments: split at a line break, `;`, `&` and `|` (so `&&` and `||`
/// too) outside quotes, substitutions, arithmetic and parentheses, and at
/// the `)` that ends a top-level `case` pattern. The state of the quotes
/// is kept from line to line, so a string over lines is one command's and
/// its lines are none of their own; the body of each here-document
/// (`<<WORD`, `<<-WORD`, the word quoted or not) is left out whole, as its
/// lines are text a command reads, not commands. A `#` that starts a word
/// in code begins a comment to the end of its line. Inside a command the
/// text is kept as written, so [`takes_root`] reads its quotes and
/// substitutions. What it does not know (`${ }`'s own quoting, an alias)
/// may refuse a script, never make text a command.
fn top_level_commands(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut levels = vec![Level {
        nest: Nest::Group,
        cases: 0,
    }];
    // The here-documents whose bodies start after the line ends: each word
    // and whether its lines' leading tabs are stripped (`<<-`).
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut word_start = true;
    // The character before, when it was no escape's.
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let top = levels.len() - 1;
        let nest = levels[top].nest;
        if c == '\\' {
            // An escaped character, a line break included, is the word's.
            current.extend(&chars[i..(i + 2).min(chars.len())]);
            i += 2;
            word_start = false;
            prev = None;
            continue;
        }
        match nest {
            Nest::Double | Nest::Back => {
                match (nest, c) {
                    (Nest::Double, '"') | (Nest::Back, '`') => {
                        levels.pop();
                    }
                    (Nest::Double, '(') if prev == Some('$') => levels.push(Level {
                        nest: if chars.get(i + 1) == Some(&'(') {
                            Nest::Arith
                        } else {
                            Nest::Subst
                        },
                        cases: 0,
                    }),
                    (Nest::Double, '`') => levels.push(Level {
                        nest: Nest::Back,
                        cases: 0,
                    }),
                    _ => {}
                }
                current.push(c);
                i += 1;
                word_start = false;
                prev = Some(c);
                continue;
            }
            Nest::Arith => {
                match c {
                    '(' => levels.push(Level {
                        nest: Nest::Arith,
                        cases: 0,
                    }),
                    ')' => {
                        levels.pop();
                    }
                    _ => {}
                }
                current.push(c);
                i += 1;
                word_start = false;
                prev = Some(c);
                continue;
            }
            Nest::Group | Nest::Subst => {}
        }
        if word_start && command_start(&current) {
            if word_at(&chars, i, "case") {
                levels[top].cases += 1;
            } else if word_at(&chars, i, "esac") {
                levels[top].cases = levels[top].cases.saturating_sub(1);
            }
        }
        match c {
            '\'' => {
                // To the closing quote, over lines; `$'…'` takes escapes.
                let ansi = prev == Some('$');
                let mut end = i + 1;
                while end < chars.len() && chars[end] != '\'' {
                    end += if ansi && chars[end] == '\\' { 2 } else { 1 };
                }
                let end = (end + 1).min(chars.len());
                current.extend(&chars[i..end]);
                i = end;
                word_start = false;
                prev = Some('\'');
                continue;
            }
            '#' if word_start => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                let end = if chars.get(i + 2) == Some(&'<') {
                    // A here-string, no document.
                    i + 3
                } else {
                    // An empty word (`<<''`) is one too: its body ends at
                    // the first empty line.
                    let (word, strip, end) = heredoc_word(&chars, i + 2);
                    heredocs.push((word, strip));
                    end
                };
                current.extend(&chars[i..end]);
                i = end;
                word_start = false;
                prev = None;
                continue;
            }
            '\n' => {
                i += 1;
                for (word, strip) in heredocs.drain(..) {
                    i = past_heredoc(&chars, i, &word, strip);
                }
                if top == 0 {
                    commands.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
                word_start = true;
                prev = None;
                continue;
            }
            ';' | '&' | '|' if top == 0 => {
                commands.push(std::mem::take(&mut current));
                i += 1;
                word_start = true;
                prev = None;
                continue;
            }
            ')' if levels[top].cases > 0 => {
                // A `case` pattern ends: the command of its branch starts.
                i += 1;
                if top == 0 {
                    commands.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
                word_start = true;
                prev = None;
                continue;
            }
            _ => {}
        }
        let mut starts = blank(c) || matches!(c, ';' | '&' | '|' | '<' | '>');
        match c {
            '"' => levels.push(Level {
                nest: Nest::Double,
                cases: 0,
            }),
            '`' => levels.push(Level {
                nest: Nest::Back,
                cases: 0,
            }),
            '(' => {
                let nest = if chars.get(i + 1) == Some(&'(') && (prev == Some('$') || word_start) {
                    Nest::Arith
                } else if matches!(prev, Some('$' | '<' | '>')) {
                    Nest::Subst
                } else {
                    Nest::Group
                };
                levels.push(Level { nest, cases: 0 });
                starts = true;
            }
            ')' if top > 0 => {
                starts = levels.pop().is_some_and(|level| level.nest == Nest::Group);
            }
            _ => {}
        }
        current.push(c);
        word_start = starts;
        prev = Some(c);
        i += 1;
    }
    commands.push(current);
    commands
}

/// The word of a here-document whose operator's `<<` ends before `at`:
/// the word with its quotes and escapes taken off, whether it is `<<-`,
/// and where the word ends.
fn heredoc_word(chars: &[char], mut at: usize) -> (String, bool, usize) {
    let strip = chars.get(at) == Some(&'-');
    if strip {
        at += 1;
    }
    while matches!(chars.get(at), Some(' ' | '\t')) {
        at += 1;
    }
    let mut word = String::new();
    while let Some(&c) = chars.get(at) {
        match c {
            '\'' | '"' => {
                at += 1;
                while let Some(&q) = chars.get(at) {
                    at += 1;
                    if q == c {
                        break;
                    }
                    word.push(q);
                }
            }
            '\\' => {
                if let Some(&next) = chars.get(at + 1) {
                    word.push(next);
                }
                at = (at + 2).min(chars.len());
            }
            c if blank(c) || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')') => {
                break;
            }
            c => {
                word.push(c);
                at += 1;
            }
        }
    }
    (word, strip, at)
}

/// Where the text goes on after a here-document whose body starts at
/// `at`: past the line that is its `word` alone (its leading tabs taken
/// off for `<<-`), or at the end of the text without one.
fn past_heredoc(chars: &[char], mut at: usize, word: &str, strip: bool) -> usize {
    while at < chars.len() {
        let end = chars[at..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |found| at + found);
        let line: String = chars[at..end].iter().collect();
        at = (end + 1).min(chars.len());
        let line = if strip {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if line == word {
            break;
        }
    }
    at
}

/// Whether one `command` assigns the cwd's Git root to a variable or `cd`s
/// to it ([`takes_root_from_cwd`]).
fn takes_root(command: &str) -> bool {
    let mut rest = command.trim_matches(blank);
    let mut declared = false;
    loop {
        let keyword = ["if", "then", "do", "else", "while", "until", "{", "!"]
            .iter()
            .find_map(|word| {
                rest.strip_prefix(word)
                    .filter(|after| after.starts_with(blank))
            });
        let declaration = ["export", "local", "readonly", "declare", "typeset"]
            .iter()
            .find_map(|word| {
                rest.strip_prefix(word)
                    .filter(|after| after.starts_with(blank))
            });
        // A declaration's options (`local -r`, `declare -x`) come before
        // its names.
        let option = rest
            .strip_prefix('-')
            .filter(|_| declared)
            .and_then(|after| after.split_once(blank))
            .map(|(_, after)| after);
        match keyword.or(declaration).or(option) {
            Some(after) => {
                declared |= declaration.is_some();
                rest = after.trim_start_matches(blank);
            }
            None => break,
        }
    }
    let value = if let Some(target) = rest.strip_prefix("cd") {
        if !target.starts_with(blank) {
            return false;
        }
        let target = target.trim_start_matches(blank);
        // `cd -- DIR`: the end of the options.
        target
            .strip_prefix("--")
            .filter(|after| after.starts_with(blank))
            .map_or(target, |after| after.trim_start_matches(blank))
    } else {
        match rest.split_once('=') {
            Some((name, value))
                if !name.is_empty()
                    && !name.starts_with(|c: char| c.is_ascii_digit())
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                value
            }
            _ => return false,
        }
    };
    // The whole value, or the whole argument of `cd`, is the output itself:
    // `"$(…)"` or `$(…)`, nothing joined before or after it (`$(…)/..`
    // names another directory) and no word after it (`root=$(…) cmd` sets
    // `root` for `cmd` alone).
    let (quoted, value) = match value.strip_prefix('"') {
        Some(inner) => (true, inner),
        None => (false, value),
    };
    let Some((substitution, after)) = value
        .strip_prefix("$(")
        .and_then(|inner| inner.split_once(')'))
    else {
        return false;
    };
    let after = if quoted {
        match after.strip_prefix('"') {
            Some(after) => after,
            None => return false,
        }
    } else {
        after
    };
    if !after.chars().all(blank) {
        return false;
    }
    // [`ROOT_FROM_CWD`] word for word, and nothing after it but its errors
    // sent to `/dev/null`: another option or argument changes what it
    // prints.
    let words: Vec<&str> = substitution
        .split(blank)
        .filter(|word| !word.is_empty())
        .collect();
    let root: Vec<&str> = ROOT_FROM_CWD.split(' ').collect();
    words.starts_with(&root)
        && matches!(
            words[root.len()..],
            [] | ["2>/dev/null"] | ["2>", "/dev/null"]
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str) -> Change {
        Change {
            path: path.to_owned(),
            status: ChangeStatus::Added,
        }
    }

    #[test]
    fn the_job_writes_dagq_toml_and_under_dagq_review_only() {
        for path in [
            "dagq.toml",
            ".dagq/review/builtin-security.md",
            ".dagq/review/programs/check-docs.sh",
        ] {
            assert!(writable(path), "{path}");
        }
        for path in [
            "AGENTS.md",
            ".dagq/review",
            ".dagq/reviewer/x.md",
            ".dagq/agents/a/AGENT.md",
            "sub/dagq.toml",
            "scripts/check.sh",
            ".git",
            ".dagq/review/.git/config",
            ".dagq/review/.GIT/hooks/post-checkout",
            ".dagq/review/../../x",
            ".dagq/review//x.md",
        ] {
            assert!(!writable(path), "{path}");
        }
        assert!(check_paths(&[change("dagq.toml"), change(".dagq/review/a.md")]).is_ok());
        let error = check_paths(&[
            change("dagq.toml"),
            change("src/lib.rs"),
            change(".dagq/agents/x/AGENT.md"),
        ])
        .unwrap_err();
        assert!(
            error.ends_with(": src/lib.rs, .dagq/agents/x/AGENT.md"),
            "{error}"
        );
    }

    #[test]
    fn only_the_builtins_and_the_programs_tables_are_generated() {
        for name in [
            "review.subagents.\"builtin:security\"",
            "review.subagents.'builtin:tests'",
            "review.subagents.\"builtin:acceptance\"",
            "review.programs",
            "review.programs.check-docs",
            "review.programs.\"builtin:secrets\"",
        ] {
            assert!(generated_table(name), "{name}");
        }
        for name in [
            "",
            "review.subagents",
            "review.subagents.design",
            "review.jobs",
            "review.programsx",
            "roles.review",
            "run.env",
        ] {
            assert!(!generated_table(name), "{name}");
        }
    }

    const BEFORE: &str = "\
# the repository's settings
[run.env]
A = \"1\"

[review.subagents.design]
paths = [\"docs/**\"]
";

    #[test]
    fn a_change_of_the_generated_tables_or_of_comments_passes_and_any_other_is_refused() {
        let after = format!(
            "{BEFORE}\n# written by setup-review\n[review.subagents.\"builtin:tests\"]\nenabled = true\n\
             [review.programs.check-docs]\nscript = \".dagq/review/programs/check-docs.sh\"\npaths = [\"docs/**\"]\n"
        );
        assert_eq!(check_config(Some(BEFORE), Some(&after)), Ok(()));
        assert_eq!(
            check_config(None, Some("[review.programs.x]\nscript = \"a.sh\"\n")),
            Ok(())
        );
        let reordered =
            "[review.subagents.design]\npaths = [\"docs/**\"]\n# moved\n[run.env]\nA = \"1\"\n";
        assert_eq!(check_config(Some(BEFORE), Some(reordered)), Ok(()));
        let error = check_config(
            Some(BEFORE),
            Some(&BEFORE.replace("A = \"1\"", "A = \"2\"")),
        )
        .unwrap_err();
        assert!(error.ends_with(": [run.env]"), "{error}");
        let error = check_config(
            Some(BEFORE),
            Some(&format!(
                "top = 1\n{BEFORE}[roles.review]\nprovider = \"codex\"\n"
            )),
        )
        .unwrap_err();
        assert!(
            error.contains("the keys before the first table") && error.contains("[roles.review]"),
            "{error}"
        );
        // Removing the file drops its tables.
        assert!(check_config(Some(BEFORE), None).is_err());
    }

    #[test]
    fn a_files_diff_is_made_from_its_bytes() {
        let added = file_diff("a.md", None, Some((b"one\ntwo\n", false)));
        assert_eq!(
            added,
            "diff a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+one\n+two\n"
        );
        let deleted = file_diff("s.sh", Some((b"x", true)), None);
        assert_eq!(
            deleted,
            "diff a/s.sh b/s.sh\ndeleted file mode 100755\n--- a/s.sh\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n\\ No newline at end of file\n"
        );
        let old: String = (1..=10).map(|n| format!("{n}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("9\n", "");
        assert_eq!(
            file_diff(
                "n",
                Some((old.as_bytes(), false)),
                Some((new.as_bytes(), false))
            ),
            "diff a/n b/n\n--- a/n\n+++ b/n\n@@ -1,10 +1,9 @@\n 1\n-2\n+two\n 3\n 4\n 5\n 6\n 7\n 8\n-9\n 10\n"
        );
        let lines = |changed: &[(usize, &str)]| -> String {
            (1..=20)
                .map(|n| {
                    changed
                        .iter()
                        .find(|(at, _)| *at == n)
                        .map_or(format!("{n}\n"), |(_, line)| format!("{line}\n"))
                })
                .collect()
        };
        let far = lines(&[]);
        let changed = lines(&[(2, "B"), (19, "S")]);
        let diff = file_diff(
            "f",
            Some((far.as_bytes(), false)),
            Some((changed.as_bytes(), false)),
        );
        assert!(
            diff.contains("@@ -1,5 +1,5 @@\n 1\n-2\n+B\n 3\n 4\n 5\n"),
            "{diff}"
        );
        assert!(
            diff.contains("@@ -16,5 +16,5 @@\n 16\n 17\n 18\n-19\n+S\n 20\n"),
            "{diff}"
        );
        assert_eq!(
            file_diff("m", Some((b"x\n", false)), Some((b"x\n", true))),
            "diff a/m b/m\nold mode 100644\nnew mode 100755\n"
        );
        assert!(
            file_diff("b", Some((b"a", false)), Some((&[0xff, 0xfe], false)))
                .ends_with("Binary files a/b and b/b differ (1 bytes, 2 bytes)\n")
        );
    }

    #[test]
    fn the_diff_shows_control_characters_as_their_escapes() {
        assert_eq!(visible("+a\tb\n-c\n"), "+a\tb\n-c\n");
        assert_eq!(
            visible("+ok\x1b[2K\x1b[1A\rhidden\u{9b}31m\x08\x7f\n"),
            "+ok\\x1b[2K\\x1b[1A\\rhidden\\x9b31m\\x08\\x7f\n"
        );
        assert_eq!(visible("+a\u{202e}b\u{2066}"), "+a\\u{202e}b\\u{2066}");
        assert_eq!(visible("+日本語"), "+日本語");
    }

    #[test]
    fn a_program_script_takes_its_root_from_the_cwd() {
        assert!(is_program_script(".dagq/review/programs/check-docs.sh"));
        assert!(!is_program_script(".dagq/review/programs/README.md"));
        assert!(!is_program_script(".dagq/review/check.sh"));
        assert!(takes_root_from_cwd(
            "#!/bin/sh\nroot=$(git rev-parse --show-toplevel) || exit 2\ncd \"$root\"\n"
        ));
        assert!(!takes_root_from_cwd(
            "#!/bin/sh\n# not git rev-parse --show-toplevel\ncd \"$(dirname \"$0\")/../..\"\n"
        ));
        for taking in [
            "#!/bin/sh\n[ $# -eq 0 ] && root=$(git rev-parse --show-toplevel)\n",
            "root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd \"$(dirname \"$0\")/..\" && pwd)\n",
            "cd \"$(git rev-parse --show-toplevel)\" || exit 2\n",
            "if ROOT=\"$( git rev-parse --show-toplevel )\"; then cd \"$ROOT\"; fi\n",
            "export root=$(git rev-parse --show-toplevel) # the cwd's tree\n",
        ] {
            assert!(takes_root_from_cwd(taking), "{taking}");
        }
        for not_taking in [
            "root=$(dirname \"$0\") # git rev-parse --show-toplevel\n",
            "echo 'root=$(git rev-parse --show-toplevel)'\n",
            "echo root=$(git rev-parse --show-toplevel)\n",
            "x=\"git rev-parse --show-toplevel\"\n",
            "root=$(echo git rev-parse --show-toplevel)\n",
            "root=$(cd \"$(dirname \"$0\")\" && git rev-parse --show-toplevel)\n",
            "cdx $(git rev-parse --show-toplevel)\n",
        ] {
            assert!(!takes_root_from_cwd(not_taking), "{not_taking}");
        }
    }

    /// The quotes are read over the whole script, not line by line: the
    /// lines of a here-document's body or of a string over lines are no
    /// commands, whatever they say, and the script's root stays where the
    /// command that is one puts it.
    #[test]
    fn a_here_document_or_a_string_over_lines_takes_no_root() {
        for not_taking in [
            "root=$(dirname \"$0\")\ncat <<'DOC'\nroot=$(git rev-parse --show-toplevel)\nDOC\n",
            "root=$(dirname \"$0\")\ncat <<DOC\nroot=$(git rev-parse --show-toplevel)\nDOC\n",
            "root=$(dirname \"$0\")\ncat <<\"DOC\" >/dev/null\nroot=$(git rev-parse --show-toplevel)\nDOC\n",
            "root=$(dirname \"$0\")\n\tcat <<-DOC\n\troot=$(git rev-parse --show-toplevel)\n\tDOC\n",
            "cat <<A; cat <<B\nx\nA\nroot=$(git rev-parse --show-toplevel)\nB\n",
            "cat <<'DOC'\nroot=$(git rev-parse --show-toplevel)\n",
            "echo \"\nroot=$(git rev-parse --show-toplevel)\n\"\n",
            "echo '\nroot=$(git rev-parse --show-toplevel)\n'\n",
            ": '\ncd \"$(git rev-parse --show-toplevel)\"\n'\n",
            "x=\"$(printf '%s\n' \"\nroot=$(git rev-parse --show-toplevel)\n\")\"\n",
            "echo `\nroot=$(git rev-parse --show-toplevel)\n`\n",
            // A `#` right after `$( )` is the word's, no comment.
            "x=$(date)#'\nroot=$(git rev-parse --show-toplevel)\n'\n",
            // `<<` in arithmetic is a shift, no here-document.
            "n=$((1<<2))\necho \"\n2\nroot=$(git rev-parse --show-toplevel)\n\"\n",
            // A `case` pattern's `)` closes no parenthesis: the root is
            // the subshell's or the substitution's only.
            "(\n  case \"$1\" in\n    a) x=1 ;;\n  esac\n  root=$(git rev-parse --show-toplevel)\n)\n",
            "x=$(case a in\n a)\n root=$(git rev-parse --show-toplevel) ;;\nesac)\n",
            // `$'…'` takes escapes: its `\'` ends no quote.
            "echo $'it\\'s'\necho '\nroot=$(git rev-parse --show-toplevel)\n'\n",
            // A no-break space is no blank to the shell.
            "x=a\u{a0}#'\nroot=$(git rev-parse --show-toplevel)\n'\n",
            // An empty here-document word is one: its body runs to the
            // first empty line.
            "root=$(dirname \"$0\")\ncat <<''\nroot=$(git rev-parse --show-toplevel)\n\n",
            "root=$(dirname \"$0\")\ncat <<\"\"\nroot=$(git rev-parse --show-toplevel)\n",
            // The output joined to another path, or a word after it.
            "root=\"$(git rev-parse --show-toplevel)/..\"\ncd \"$root\"\n",
            "cd \"$(git rev-parse --show-toplevel)/..\"\n",
            "root=$(git rev-parse --show-toplevel)/sub\n",
            "root=$(git rev-parse --show-toplevel)x\n",
            "root=\"$(git rev-parse --show-toplevel)\"x\n",
            "root=$(git rev-parse --show-toplevel)\"\"\n",
            "root=$(git rev-parse --show-toplevel) env\n",
            "cd \"$(git rev-parse --show-toplevel)\" extra\n",
            "root=\"$(git rev-parse --show-toplevel)\n",
            // Another option or argument changes what Git prints.
            "root=$(git rev-parse --show-toplevel-not-a-real-option)\n",
            "root=$(git rev-parse --show-toplevel --git-dir)\n",
            "root=$(git rev-parse --show-toplevel HEAD)\n",
            "root=$(git -C /elsewhere rev-parse --show-toplevel)\n",
            "root=$(git rev-parse --show-toplevel 2>&1)\n",
            // A word that only reads as `esac` closes no `case`.
            "x=$(case a in\n a) echo esac ;;\n b)\nroot=$(git rev-parse --show-toplevel) ;;\nesac)\n",
        ] {
            assert!(!takes_root_from_cwd(not_taking), "{not_taking}");
        }
        for taking in [
            "cat <<'DOC'\nroot=$(dirname \"$0\")\nDOC\nroot=$(git rev-parse --show-toplevel)\n",
            "cat <<-DOC\n\tx\n\tDOC\ncd \"$(git rev-parse --show-toplevel)\"\n",
            "msg='a\nb'; root=$(git rev-parse --show-toplevel)\n",
            "grep -q x <<<\"$1\" && root=$(git rev-parse --show-toplevel)\n",
            "root=$(\n  git rev-parse --show-toplevel\n)\n",
            "echo \"done\" # git rev-parse --show-toplevel\nroot=$(git rev-parse --show-toplevel)\n",
            // A comment ends at its line, a backslash at its end or not.
            "# a comment \\\nroot=$(git rev-parse --show-toplevel)\n",
            "mask=$((1 << 3))\nroot=$(git rev-parse --show-toplevel)\n",
            "case \"$1\" in\n  -h) exit 0 ;;\n  *) root=$(git rev-parse --show-toplevel) ;;\nesac\n",
            "x=$(date)\n(cd /tmp) # it's elsewhere\nroot=$(git rev-parse --show-toplevel)\n",
            "echo hi >#x\nroot=$(git rev-parse --show-toplevel)\n",
            "cd -- \"$(git rev-parse --show-toplevel)\"\n",
            "root=\"$(git rev-parse --show-toplevel)\"  \n",
            "cat <<''\nx\n\nroot=$(git rev-parse --show-toplevel 2> /dev/null)\n",
            "f() {\n  local -r root=$(git rev-parse --show-toplevel)\n}\n",
        ] {
            assert!(takes_root_from_cwd(taking), "{taking}");
        }
    }
}
