//! The generic shape of a shell command (ADR-t1662-3 decisions 14 and 15,
//! docs/design/measurement-commands.md): what a session's step keeps of a
//! command it ran instead of its text. The command is split at pipes,
//! `&&`, `||`, `;`, `&`, newlines and a subshell's bounds into parts (a
//! `case`'s patterns and a conditional expression's `[[ … ]]` are parts of
//! their own); each part keeps its leading reserved words of the shell
//! (`lead`: `if`, `while`, `until`, `for`, `do` …), the names of the
//! variables it assigns (`assigns`, never their values, an array's
//! `NAME=(…)` too), its program's name without its path (`program`) and
//! its arguments (`args`).
//!
//! Only what a list names is kept in plain (ADR-t1673-1): a program is
//! kept by name only when a list of [`Subcommands`] names it, and any other
//! first word (an unknown program, a substitution, or a value a misread
//! split put there) is its digest, with every word after it as a `word`.
//! A listed program's arguments are each one of:
//!
//! - `{"flag": name}`: an unquoted word that starts with `-` and whose name
//!   looks like a flag's (letters, digits, `_`, `-`, `.`); `--name=value`
//!   keeps the name and the value's digest
//!   (`{"flag": "--name", "value": …}`), its value quoted or not; a short
//!   flag keeps its letter, and what is glued to it (`-pS3cr3t`) is its
//!   value. Any other word that starts with `-` (its name quoted, or with a
//!   blank or a quote in it) is a `word`, and so is one right after an
//!   option that may take it as its value: one the program's list does not
//!   name as taking no value ([`SubcommandList::switches`]), its value not
//!   attached by `=` (`grep -e --pattern`).
//! - `{"sub": word}`: one of the first two words after the program before
//!   any flag that matches `^[a-z][a-z0-9-]{0,23}$` and is one of the
//!   program's known subcommands ([`Subcommands`], ADR-t1673-1),
//!   kept as it is, unless it follows a word a secret may follow
//!   ([`SECRET_SUBS`]). Any other word there is a `word`: `printf %s
//!   hunter2`, `cargo test runtime` and `tool login hunter2` keep no word
//!   in plain.
//! - `{"word": digest}`: any other word.
//! - `{"plus": digest}`: the first word of a program that picks its
//!   toolchain by a word that starts with `+` (`cargo +nightly`,
//!   [`SubcommandList::toolchain`]), which takes no subcommand's position.
//!   Any other word that starts with `+` is an operand (`grep +pattern`).
//! - `{"after_dashdash": n}`: the separator `--`, with how many words came
//!   after it.
//!
//! A listed program that runs another command ([`Runs`]: `timeout 60 …`,
//! `xargs -n1 …`, a shell's script, `git bisect run …`, `cargo run …`)
//! keeps its own words alone: from the first word of what it runs, every
//! word is a `word`, a flag's too. So does one whose parser may stop at its
//! first operand ([`SubcommandList::options_after_operands`]: git, the
//! shell's tools) from its first word that is no flag, no toolchain and no
//! known subcommand, an option's possible value too (`git config --get-regexp
//! 'x.*' --pattern`), and a clap program from an unknown subcommand's
//! (`cargo foo …`). A command the lexer does not read whole
//! (a quote, a substitution, a subshell, a `case` or a `[[` open at its
//! end, or a `)` that closes nothing) keeps no word in plain at all: each
//! of its parts is its words' digests.
//!
//! Redirections (`2>&1`, `> out`, a heredoc and its body) and comments are
//! dropped. A shell's script (`bash -lc '…'`, as Codex runs its commands)
//! is shaped as the parts of its script. A digest is the HMAC of the word
//! under the queue's secret key ([`DigestKey`], ADR-t1673-1 decision 10):
//! one word is one digest within the queue, for matching, and one without
//! the key cannot match a guessed word to it; without a key no command is
//! shaped. Nothing here keeps a command's text, a value or a filter
//! outside a subcommand's position. The shape is bounded within
//! [`super::telemetry::MAX_SHAPE_BYTES`] as JSON ([`shape_value_with`]): what
//! does not fit is cut from the end and marked `truncated`.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// What every digested word starts with: its version, so that another
/// way of making them never matches these.
pub const DIGEST_PREFIX: &[u8] = b"dagq-cmd-v2\0";

/// How many bytes a queue's [`DigestKey`] has.
pub const DIGEST_KEY_BYTES: usize = 32;

/// How many hex digits of the SHA-256 a digest keeps.
pub const DIGEST_HEX: usize = 16;

/// How many words after the program, before any flag, may be a
/// subcommand.
pub const SUB_POSITIONS: usize = 2;

/// The words a secret may follow (`login <token>`, `AUTH <password>`), in
/// any case: the word after one is never a subcommand.
const SECRET_SUBS: [&str; 10] = [
    "auth",
    "login",
    "password",
    "passwd",
    "pass",
    "token",
    "secret",
    "key",
    "credential",
    "credentials",
];

/// How deep a shell's script inside a shell's script is shaped.
const MAX_SCRIPT_DEPTH: usize = 3;

/// The shell's reserved words a part may start with.
const RESERVED: [&str; 18] = [
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "case", "esac",
    "select", "function", "time", "!", "{", "}",
];

/// The programs whose `-c` script is shaped in their place.
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// The secret key of a queue its executors digest their commands' words
/// with (ADR-t1673-1 decision 10): one word is one digest within the
/// queue, and one who reads the steps without the key cannot match a
/// guessed word to its digest. Where it is kept and how it is made is the
/// infrastructure's ([`crate::application::TelemetrySinks::digest_key`]);
/// it is never in a step, an event or a log, and its `Debug` shows none
/// of it.
#[derive(Clone, PartialEq, Eq)]
pub struct DigestKey([u8; DIGEST_KEY_BYTES]);

impl std::fmt::Debug for DigestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DigestKey(..)")
    }
}

impl DigestKey {
    /// The key of `bytes`; `None` unless they are [`DIGEST_KEY_BYTES`].
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    /// The digest of `word`: HMAC-SHA256 under this key of
    /// [`DIGEST_PREFIX`] + word, its first [`DIGEST_HEX`] hex digits.
    pub fn digest(&self, word: &str) -> String {
        hmac_sha256(&self.0, &[DIGEST_PREFIX, word.as_bytes()])
            .iter()
            .take(DIGEST_HEX / 2)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// A key the tests digest with.
    #[cfg(test)]
    pub fn test() -> Self {
        Self([7; DIGEST_KEY_BYTES])
    }
}

/// HMAC-SHA256 (RFC 2104) under `key` (at most a block) of `message`'s
/// parts, one after the other.
fn hmac_sha256(key: &[u8], message: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    padded[..key.len().min(BLOCK)].copy_from_slice(&key[..key.len().min(BLOCK)]);
    let mut inner = Sha256::new();
    inner.update(padded.map(|byte| byte ^ 0x36));
    for part in message {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(padded.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// The shape of a command: its parts in order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct CommandShape {
    pub parts: Vec<ShapePart>,
    /// Parts or arguments were cut from the end to fit
    /// [`super::telemetry::MAX_SHAPE_BYTES`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// One part of a command (see the module).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ShapePart {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lead: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assigns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub args: Vec<ShapeArg>,
}

/// One argument of a part (see the module).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ShapeArg {
    Flag {
        flag: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Sub {
        sub: String,
    },
    Word {
        word: String,
    },
    Plus {
        plus: String,
    },
    AfterDashdash {
        after_dashdash: usize,
    },
}

/// The shape of `command`, its known subcommands and its key `known`'s;
/// `None` without a key: a word is never digested without one.
pub fn shape(command: &str, known: &Subcommands) -> Option<CommandShape> {
    known.key.as_ref()?;
    Some(CommandShape {
        parts: parts_of(command, 0, known),
        truncated: false,
    })
}

/// [`shape_value_with`] the runtime's common known subcommands alone and
/// the tests' key.
#[cfg(test)]
pub fn shape_value(command: &str) -> serde_json::Value {
    shape_value_with(
        command,
        &Subcommands::common().keyed(Some(DigestKey::test())),
    )
    .unwrap_or_default()
}

/// The shape of `command`, its known subcommands and its key `known`'s, as
/// JSON, within [`super::telemetry::MAX_SHAPE_BYTES`]: arguments, then
/// parts, cut from the end until it fits, and marked `truncated`; `None`
/// without a key, when a step keeps no shape.
pub fn shape_value_with(command: &str, known: &Subcommands) -> Option<serde_json::Value> {
    let mut shape = shape(command, known)?;
    let fits = |shape: &CommandShape| {
        serde_json::to_vec(shape)
            .is_ok_and(|bytes| bytes.len() <= super::telemetry::MAX_SHAPE_BYTES)
    };
    while !fits(&shape) {
        shape.truncated = true;
        match shape.parts.last_mut() {
            Some(part) if !part.args.is_empty() => {
                part.args.pop();
            }
            Some(_) => {
                shape.parts.pop();
            }
            None => break,
        }
    }
    serde_json::to_value(&shape).ok()
}

/// The shaped parts of `command`, a script `depth` shells deep. A command
/// not read whole ([`Lexer::broken`]) keeps no word in plain: each of its
/// parts is the digests of its words, as one that runs no program.
fn parts_of(command: &str, depth: usize, known: &Subcommands) -> Vec<ShapePart> {
    let (mut parts, broken) = Lexer::default().split(command);
    if broken {
        for first in parts.iter_mut().filter_map(|words| words.first_mut()) {
            first.pattern = true;
        }
    }
    parts
        .into_iter()
        .flat_map(|words| shape_part(&words, depth, known))
        .collect()
}

/// A word of a command, its quotes and escapes taken out, whether any of
/// it was quoted, and whether a quote began before its first `=` (a flag's
/// name quoted, not only its value: `"--test=it"`, not `--test="it"`).
#[derive(Debug, Default, Clone)]
struct Word {
    text: String,
    quoted: bool,
    name_quoted: bool,
    /// The first word of a part that runs no program: a `case` pattern,
    /// what follows a subshell's close, or any part of a command not read
    /// whole; its words are all digests.
    pattern: bool,
}

impl Word {
    /// A quote or an escape begins here.
    fn quote(&mut self) {
        self.quoted = true;
        if !self.text.contains('=') {
            self.name_quoted = true;
        }
    }
}

/// What the next word is after a redirection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// A file: dropped.
    File,
    /// A heredoc's delimiter, whose body starts at the next newline;
    /// whether its lines' leading tabs are stripped (`<<-`).
    Heredoc(bool),
}

/// Where a `case` the lexer reads stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseAt {
    /// Its subject, up to its `in`; whether the subject was read.
    Head(bool),
    /// A pattern, up to the `)` that closes it: a part that runs no
    /// program ([`Word::pattern`]), whose alternatives `|` separates.
    Pattern,
    /// An arm's commands, up to its `;;` (or `;&`, `;;&`) or the `esac`.
    Body,
}

/// A `case` open: where it stands, and the subshells open at its start.
#[derive(Debug, Clone, Copy)]
struct Case {
    at: CaseAt,
    depth: usize,
}

/// Splits a command into the words of its parts.
#[derive(Default)]
struct Lexer {
    parts: Vec<Vec<Word>>,
    words: Vec<Word>,
    word: Option<Word>,
    target: Option<Target>,
    heredocs: Vec<Heredoc>,
    /// The subshells open (`(`).
    depth: usize,
    /// The `case`s open, the innermost last.
    cases: Vec<Case>,
    /// The part being read runs no program ([`Word::pattern`]).
    pattern: bool,
    /// A conditional expression (`[[ … ]]`) is open: its `||`, `&&`, `|`,
    /// `<`, `>` and parentheses are its words', not the shell's.
    cond: bool,
    /// The command was not read whole: a quote, a substitution, a
    /// subshell, a `case` or a `[[` left open at its end, or a `)` that
    /// closes nothing. Its words may then be split wrong, and so none is
    /// kept in plain ([`parts_of`]).
    broken: bool,
}

impl Lexer {
    /// The words of `command`'s parts, and whether it was not read whole
    /// ([`Self::broken`]).
    fn split(mut self, command: &str) -> (Vec<Vec<Word>>, bool) {
        let chars: Vec<char> = command.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            // A `]]` glued to an operator (`]]&&`, `]])`) closes its
            // conditional expression before the operator is read.
            if self.cond
                && matches!(c, '|' | '&' | '(' | ')' | '<' | '>')
                && self
                    .word
                    .as_ref()
                    .is_some_and(|word| !word.quoted && word.text == "]]")
            {
                self.end_word();
            }
            match c {
                // A conditional expression goes on past its line's end.
                '\n' if self.cond => self.end_word(),
                '\n' => {
                    self.closes_case();
                    self.end_part();
                    self.pattern = false;
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
                    self.broken |= i >= chars.len();
                }
                '"' => {
                    i = self.double_quoted(&chars, i + 1);
                    continue;
                }
                '\\' => {
                    if let Some(escaped) = next {
                        if escaped != '\n' {
                            let word = self.word.get_or_insert_default();
                            word.quote();
                            word.text.push(escaped);
                        }
                        i += 1;
                    }
                }
                '$' if matches!(next, Some('(' | '{' | '[')) => {
                    let closed;
                    (i, closed) =
                        copy_group(&chars, i, &mut self.word.get_or_insert_default().text);
                    self.broken |= !closed;
                    continue;
                }
                // The process's id (`$$`): its second `$` begins no quote.
                '$' if next == Some('$') => {
                    self.word.get_or_insert_default().text.push_str("$$");
                    i += 2;
                    continue;
                }
                '$' if next == Some('\'') => {
                    i = self.ansi_quoted(&chars, i + 2);
                    continue;
                }
                '`' => {
                    let closed;
                    (i, closed) =
                        copy_backticks(&chars, i, &mut self.word.get_or_insert_default().text);
                    self.broken |= !closed;
                    continue;
                }
                '|' | '&' | '(' | ')' | '<' | '>' if self.cond => {
                    self.word.get_or_insert_default().text.push(c);
                }
                '&' if next == Some('>') => {
                    self.end_word();
                    i = self.redirect(&chars, i + 1);
                    continue;
                }
                // A pattern's alternatives.
                '|' if self.case_at() == Some(CaseAt::Pattern) && !self.closes_case() => {
                    self.end_word();
                }
                '&' | '|' | ';' => {
                    self.closes_case();
                    let arm_ends = c == ';'
                        && matches!(next, Some(';' | '&'))
                        && self.case_at() == Some(CaseAt::Body)
                        && self.case_depth() == self.depth;
                    self.end_part();
                    self.pattern = false;
                    if arm_ends {
                        self.set_case(CaseAt::Pattern);
                    }
                    if matches!(
                        (c, next),
                        ('&', Some('&')) | ('|', Some('|' | '&')) | (';', Some(';'))
                    ) {
                        i += 1;
                    }
                }
                // A function's definition (`NAME()`, `NAME ()`): its name
                // ends its part.
                '(' if next == Some(')') => {
                    self.end_part();
                    i += 2;
                    continue;
                }
                // A group glued to a word (an array's `NAME=(…)`, an
                // extglob's `!(…)`), an arithmetic `((…))`, or one after a
                // subshell's close is the word's text, kept as its digest.
                '(' if self.word.is_some() || next == Some('(') || self.pattern => {
                    // An arithmetic command (`((…))`) ends there: a `#`
                    // glued after it begins a comment.
                    let arithmetic = self.word.is_none() && next == Some('(');
                    let word = self.word.get_or_insert_default();
                    let closed;
                    (i, closed) = copy_parens(&chars, i, &mut word.text);
                    self.broken |= !closed;
                    if arithmetic {
                        self.end_word();
                    }
                    continue;
                }
                // A pattern's optional open.
                '(' if self.case_at() == Some(CaseAt::Pattern) => {}
                '(' => {
                    self.end_part();
                    self.depth += 1;
                }
                ')' => {
                    let closed = self.closes_case();
                    if !closed && self.case_at() == Some(CaseAt::Pattern) {
                        self.end_part();
                        self.set_case(CaseAt::Body);
                    } else {
                        // One no `(` opened ends a part that runs no
                        // program, and the command was misread; what
                        // follows a subshell's close before the next
                        // operator runs none either.
                        let closes = self.depth > self.case_depth();
                        if closes {
                            self.depth -= 1;
                        }
                        self.broken |= !closes;
                        self.pattern |= !closes;
                        self.end_part();
                        self.pattern = closes;
                    }
                }
                // A process substitution (`<(…)`, `>(…)`) is a word of its
                // own.
                '>' | '<' if next == Some('(') => {
                    self.end_word();
                    let word = self.word.get_or_insert_default();
                    let closed;
                    (i, closed) = copy_parens(&chars, i + 1, &mut word.text);
                    self.broken |= !closed;
                    continue;
                }
                '>' | '<' => {
                    // A word of digits before it is the descriptor it
                    // redirects (`2>`), dropped with it, unless it is the
                    // target of the redirection before (a heredoc's
                    // delimiter `1` in `<<1>out`).
                    let descriptor = self.target.is_none()
                        && self.word.as_ref().is_none_or(|word| {
                            !word.quoted && word.text.chars().all(|c| c.is_ascii_digit())
                        });
                    if descriptor {
                        self.word = None;
                    } else {
                        self.end_word();
                    }
                    i = self.redirect(&chars, i);
                    continue;
                }
                _ => self.word.get_or_insert_default().text.push(c),
            }
            i += 1;
        }
        self.closes_case();
        self.broken |= self.cond || self.depth > 0 || !self.cases.is_empty();
        self.end_part();
        (self.parts, self.broken)
    }

    /// Read the redirection operator at `i` (after any `&` of `&>`); the
    /// index after it. A target that follows (a file, a heredoc's
    /// delimiter) is the next word, which is not kept.
    fn redirect(&mut self, chars: &[char], mut i: usize) -> usize {
        let first = chars[i];
        i += 1;
        let at = |i: usize| chars.get(i).copied();
        if first == '<' && at(i) == Some('<') {
            i += 1;
            if at(i) == Some('<') {
                // A here-string: its word is dropped.
                self.target = Some(Target::File);
                return i + 1;
            }
            let strip = at(i) == Some('-');
            if strip {
                i += 1;
            }
            self.target = Some(Target::Heredoc(strip));
            return i;
        }
        if matches!(at(i), Some('>' | '|')) || (first == '<' && at(i) == Some('>')) {
            i += 1;
        }
        if at(i) == Some('&') {
            i += 1;
            // `>&1`, `<&-`: a descriptor, no file.
            if at(i).is_some_and(|c| c.is_ascii_digit() || c == '-') {
                while at(i).is_some_and(|c| c.is_ascii_digit() || c == '-') {
                    i += 1;
                }
                return i;
            }
        }
        self.target = Some(Target::File);
        i
    }

    /// Read an ANSI-C quoted text (`$'…'`) from `i` (after its `$'`) into
    /// the word, its escapes decoded as the shell does (`\x27` is a `'`,
    /// so a shell's `-c` script is read as the shell reads it) and `\'` not
    /// closing it; the index after its closing `'`.
    fn ansi_quoted(&mut self, chars: &[char], mut i: usize) -> usize {
        let word = self.word.get_or_insert_default();
        word.quote();
        while i < chars.len() {
            match chars[i] {
                '\'' => return i + 1,
                '\\' => i = ansi_escape(chars, i + 1, &mut word.text),
                c => {
                    word.text.push(c);
                    i += 1;
                }
            }
        }
        self.broken = true;
        i
    }

    /// Read a double-quoted text from `i` (after its `"`) into the word;
    /// the index after its closing `"`.
    fn double_quoted(&mut self, chars: &[char], mut i: usize) -> usize {
        let word = self.word.get_or_insert_default();
        word.quote();
        while i < chars.len() {
            match chars[i] {
                '"' => return i + 1,
                '\\' if matches!(chars.get(i + 1), Some('"' | '\\' | '$' | '`' | '\n')) => {
                    if chars[i + 1] != '\n' {
                        word.text.push(chars[i + 1]);
                    }
                    i += 2;
                }
                '$' if matches!(chars.get(i + 1), Some('(' | '{' | '[')) => {
                    let closed;
                    (i, closed) = copy_group(chars, i, &mut word.text);
                    self.broken |= !closed;
                }
                '`' => {
                    let closed;
                    (i, closed) = copy_backticks(chars, i, &mut word.text);
                    self.broken |= !closed;
                }
                c => {
                    word.text.push(c);
                    i += 1;
                }
            }
        }
        self.broken = true;
        i
    }

    /// Skip the bodies of the heredocs whose delimiters were read, from
    /// `i` (the start of the line after theirs); the index after the last.
    fn skip_heredocs(&mut self, chars: &[char], mut i: usize) -> usize {
        for heredoc in std::mem::take(&mut self.heredocs) {
            i = heredoc_end(chars, i, &heredoc);
        }
        i
    }

    fn end_word(&mut self) {
        let Some(word) = self.word.take() else {
            return;
        };
        match self.target.take() {
            Some(Target::Heredoc(strip)) => self.heredocs.push(Heredoc {
                delimiter: word.text,
                strip,
                quoted: word.quoted,
            }),
            Some(Target::File) => {}
            None => {
                let plain = |text: &str| !word.quoted && word.text == text;
                if self.cond {
                    self.cond = !plain("]]");
                    self.words.push(word);
                    return;
                }
                // After reserved words alone, or after a function's
                // definition (`function NAME`) and them.
                let before = match self.words.as_slice() {
                    [function, _, rest @ ..] if !function.quoted && function.text == "function" => {
                        rest
                    }
                    words => words,
                };
                let command_position = !self.pattern
                    && before.iter().all(|word| {
                        !word.quoted
                            && RESERVED.contains(&word.text.as_str())
                            && !matches!(word.text.as_str(), "for" | "select" | "case" | "function")
                    });
                if plain("[[") && command_position && self.case_at() != Some(CaseAt::Pattern) {
                    self.cond = true;
                }
                match self.case_at() {
                    Some(CaseAt::Head(false)) => self.set_case(CaseAt::Head(true)),
                    // The case's head ends at its `in`: its patterns follow.
                    Some(CaseAt::Head(true)) if plain("in") => {
                        self.words.push(word);
                        self.parts.push(std::mem::take(&mut self.words));
                        self.set_case(CaseAt::Pattern);
                        return;
                    }
                    Some(CaseAt::Head(true) | CaseAt::Pattern) => {}
                    _ if plain("case") && command_position => {
                        self.cases.push(Case {
                            at: CaseAt::Head(false),
                            depth: self.depth,
                        });
                    }
                    _ => {}
                }
                self.words.push(word);
            }
        }
    }

    /// Where the innermost `case` open stands.
    fn case_at(&self) -> Option<CaseAt> {
        self.cases.last().map(|case| case.at)
    }

    fn set_case(&mut self, at: CaseAt) {
        if let Some(case) = self.cases.last_mut() {
            case.at = at;
        }
    }

    /// The subshells open at the innermost `case`'s start: a `)` closes a
    /// subshell only past them.
    fn case_depth(&self) -> usize {
        self.cases.last().map_or(0, |case| case.depth)
    }

    /// Whether the words of the part read so far are an `esac` alone,
    /// past a `case`'s head and in no subshell its arm opened: that `case`
    /// ends here.
    fn closes_case(&mut self) -> bool {
        self.end_word();
        let closes =
            self.cases.last().is_some_and(|case| {
                !matches!(case.at, CaseAt::Head(_)) && case.depth == self.depth
            }) && matches!(self.words.as_slice(), [word] if !word.quoted && word.text == "esac");
        if closes {
            self.cases.pop();
        }
        closes
    }

    fn end_part(&mut self) {
        self.end_word();
        self.target = None;
        self.cond = false;
        let pattern = std::mem::take(&mut self.pattern) || self.case_at() == Some(CaseAt::Pattern);
        if let Some(first) = self.words.first_mut() {
            first.pattern = pattern;
            self.parts.push(std::mem::take(&mut self.words));
        }
    }
}

/// A heredoc whose delimiter was read: the delimiter, its quotes and
/// escapes taken out; whether its lines' leading tabs are stripped
/// (`<<-`); and whether any of its delimiter was quoted or escaped.
#[derive(Debug, Clone)]
struct Heredoc {
    delimiter: String,
    strip: bool,
    quoted: bool,
}

/// The index after the body of `heredoc` that starts at `i`: after the
/// line that is its delimiter, or the end. Its lines are read as the shell
/// reads them: when its delimiter is unquoted, a line that ends with an odd
/// number of `\` goes on at the next (`x\` then `EOF` is the line `xEOF`,
/// which does not end it, and `E\` then `OF` is `EOF`, which does), and
/// `<<-` strips the tabs that lead its first line alone.
fn heredoc_end(chars: &[char], mut i: usize, heredoc: &Heredoc) -> usize {
    while i < chars.len() {
        let mut line = String::new();
        let mut first = true;
        loop {
            let end = chars[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |at| i + at);
            let mut physical = &chars[i..end];
            if heredoc.strip && first {
                let tabs = physical.iter().take_while(|&&c| c == '\t').count();
                physical = &physical[tabs..];
            }
            first = false;
            i = (end + 1).min(chars.len());
            let escapes = physical.iter().rev().take_while(|&&c| c == '\\').count();
            if !heredoc.quoted && escapes % 2 == 1 && end < chars.len() {
                line.extend(&physical[..physical.len() - 1]);
                continue;
            }
            line.extend(physical);
            break;
        }
        if line == heredoc.delimiter {
            break;
        }
    }
    i
}

/// Copy the `$(…)` or `${…}` at `i` into `text`, its nesting, quotes,
/// comments and heredocs included (a heredoc's body and a comment are
/// copied as they are, so neither their parentheses nor their quotes end
/// the group, and a substitution inside a double-quoted text is a group of
/// its own); the index after its close, and whether it closed.
fn copy_group(chars: &[char], mut i: usize, text: &mut String) -> (usize, bool) {
    let (open, close) = match chars.get(i + 1) {
        Some('{') => ('{', '}'),
        Some('[') => ('[', ']'),
        _ => ('(', ')'),
    };
    // An arithmetic expansion (`$((…))`, `$[…]`) has shifts, no heredocs.
    let arithmetic = open == '[' || (open == '(' && chars.get(i + 2) == Some(&'('));
    text.push('$');
    i += 1;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut heredocs: Vec<Heredoc> = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        text.push(c);
        i += 1;
        // A here-string (`<<<`) and a shift's assignment (`<<=`) are no
        // heredoc.
        if quote.is_none() && c == '<' && chars.get(i) == Some(&'<') && arithmetic {
            text.push('<');
            i += 1;
            continue;
        }
        if quote.is_none()
            && c == '<'
            && chars.get(i) == Some(&'<')
            && matches!(chars.get(i + 1), Some('<' | '='))
        {
            text.push('<');
            text.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if quote.is_none() && c == '<' && chars.get(i) == Some(&'<') {
            text.push('<');
            i += 1;
            let strip = chars.get(i) == Some(&'-');
            if strip {
                text.push('-');
                i += 1;
            }
            while let Some(&blank @ (' ' | '\t')) = chars.get(i) {
                text.push(blank);
                i += 1;
            }
            // The delimiter, its quotes taken out: a blank or an operator
            // inside a quote is its own.
            let mut delimiter = String::new();
            // An empty quoted delimiter (`<<''`) is one: its body ends at
            // an empty line.
            let mut read = false;
            let mut quoted = false;
            while let Some(&c) = chars.get(i) {
                if c.is_whitespace() || matches!(c, ';' | ')' | '(' | '|' | '&' | '<' | '>') {
                    break;
                }
                read = true;
                text.push(c);
                i += 1;
                quoted |= matches!(c, '\'' | '"' | '\\');
                match c {
                    '\'' | '"' => {
                        while let Some(&quoted) = chars.get(i) {
                            text.push(quoted);
                            i += 1;
                            if quoted == c {
                                break;
                            }
                            delimiter.push(quoted);
                        }
                    }
                    '\\' => {
                        if let Some(&escaped) = chars.get(i) {
                            text.push(escaped);
                            delimiter.push(escaped);
                            i += 1;
                        }
                    }
                    c => delimiter.push(c),
                }
            }
            if read {
                heredocs.push(Heredoc {
                    delimiter,
                    strip,
                    quoted,
                });
            }
            continue;
        }
        if quote.is_none() && c == '\n' {
            for heredoc in std::mem::take(&mut heredocs) {
                let end = heredoc_end(chars, i, &heredoc);
                text.extend(&chars[i..end]);
                i = end;
            }
            continue;
        }
        // A substitution (`$(…)`, `${…}`, `` `…` ``), unquoted or inside a
        // double-quoted text, is a group of its own: its quotes and its
        // close are its own.
        let nests = matches!(quote, None | Some('"'))
            && (c == '`' || (c == '$' && matches!(chars.get(i), Some('(' | '{' | '['))));
        if nests {
            text.pop();
            let closed;
            (i, closed) = if c == '`' {
                copy_backticks(chars, i - 1, text)
            } else {
                copy_group(chars, i - 1, text)
            };
            if !closed {
                return (i, false);
            }
            continue;
        }
        match quote {
            // An ANSI-C quoted text (`$'…'`), where `\'` does not close.
            Some('$') if c == '\\' => {
                if let Some(&escaped) = chars.get(i) {
                    text.push(escaped);
                    i += 1;
                }
            }
            Some('$') if c == '\'' => quote = None,
            Some('$') => {}
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(&escaped) = chars.get(i) {
                    text.push(escaped);
                    i += 1;
                }
            }
            Some(_) => {}
            // An escaped character (`\)`) neither quotes nor closes.
            None if c == '\\' => {
                if let Some(&escaped) = chars.get(i) {
                    text.push(escaped);
                    i += 1;
                }
            }
            // The process's id (`$$`): its second `$` begins no quote.
            None if c == '$' && chars.get(i) == Some(&'$') => {
                text.push('$');
                i += 1;
            }
            None if c == '$' && chars.get(i) == Some(&'\'') => {
                text.push('\'');
                i += 1;
                quote = Some('$');
            }
            None if c == '\'' || c == '"' => quote = Some(c),
            // A comment, to its line's end, in a group of commands.
            None if c == '#'
                && open == '('
                && i >= 2
                && matches!(
                    chars[i - 2],
                    ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')'
                ) =>
            {
                while let Some(&next) = chars.get(i).filter(|&&next| next != '\n') {
                    text.push(next);
                    i += 1;
                }
            }
            None if c == open => depth += 1,
            None if c == close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (i, true);
                }
            }
            None => {}
        }
    }
    (i, false)
}

/// Copy the `(…)` whose `(` is at `open` into `text`, as [`copy_group`]
/// copies a `$(…)`; the index after its close, and whether it closed.
fn copy_parens(chars: &[char], open: usize, text: &mut String) -> (usize, bool) {
    let mut group = Vec::with_capacity(chars.len() - open + 1);
    group.push('$');
    group.extend_from_slice(&chars[open..]);
    let (end, closed) = copy_group(&group, 0, text);
    (open + end - 1, closed)
}

/// Copy the `` `…` `` at `i` into `text`; the index after its close, and
/// whether it closed.
fn copy_backticks(chars: &[char], mut i: usize, text: &mut String) -> (usize, bool) {
    text.push('`');
    i += 1;
    while i < chars.len() {
        let c = chars[i];
        text.push(c);
        i += 1;
        if c == '\\' {
            if let Some(&escaped) = chars.get(i) {
                text.push(escaped);
                i += 1;
            }
        } else if c == '`' {
            return (i, true);
        }
    }
    (i, false)
}

/// Decode the escape of an ANSI-C quoted text whose `\` is before `i`
/// into `text`, as the shell does; the index after it. An escape the
/// shell does not know keeps its `\`.
fn ansi_escape(chars: &[char], mut i: usize, text: &mut String) -> usize {
    let Some(&c) = chars.get(i) else {
        text.push('\\');
        return i;
    };
    i += 1;
    // Up to `max` digits of `radix` from `i`, as a character.
    let number = |i: &mut usize, radix: u32, max: usize| {
        let start = *i;
        while *i < chars.len() && *i - start < max && chars[*i].is_digit(radix) {
            *i += 1;
        }
        let digits: String = chars[start..*i].iter().collect();
        u32::from_str_radix(&digits, radix)
            .ok()
            .and_then(char::from_u32)
    };
    let decoded = match c {
        'a' => Some('\u{7}'),
        'b' => Some('\u{8}'),
        'e' | 'E' => Some('\u{1b}'),
        'f' => Some('\u{c}'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'v' => Some('\u{b}'),
        '\\' | '\'' | '"' | '?' => Some(c),
        'x' => number(&mut i, 16, 2),
        'u' => number(&mut i, 16, 4),
        'U' => number(&mut i, 16, 8),
        '0'..='7' => {
            i -= 1;
            number(&mut i, 8, 3)
        }
        // A `\` escapes one character alone: `$'\c'` ends at its `'`.
        'c' => chars
            .get(i)
            .filter(|&&control| control != '\'')
            .map(|&control| {
                i += 1;
                char::from(control as u8 & 0x1f)
            }),
        _ => None,
    };
    match decoded {
        Some(decoded) => text.push(decoded),
        None => {
            text.push('\\');
            text.push(c);
        }
    }
    i
}

/// Whether `text` assigns a variable (`NAME=…`): the name, when it does.
fn assigned(text: &str) -> Option<&str> {
    let (name, _) = text.split_once('=')?;
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

/// Whether `name` is a flag's name: `-` or `--`, then a letter or digit
/// and letters, digits, `_`, `-` and `.`.
fn flag_name(name: &str) -> bool {
    let rest = name.strip_prefix("--").or_else(|| name.strip_prefix('-'));
    rest.is_some_and(|rest| {
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    })
}

/// Programs a shape keeps by name, and what of their words it keeps
/// (ADR-t1673-1).
#[derive(Debug, Clone, Copy)]
pub struct SubcommandList {
    /// The programs' names, without their paths.
    pub programs: &'static [&'static str],
    /// Their known subcommands, kept as they are at a subcommand's
    /// position: the fixed words of their first two levels (`cargo nextest
    /// run`, `git stash push`), none for a program that has none.
    pub subcommands: &'static [&'static str],
    /// Their options that take no value (`--locked`, `-v`): a word right
    /// after one is never its value, so a flag there keeps its name. A flag
    /// right after any other option (one that may take a value, its value
    /// not attached by `=`) is taken as that option's value, a digest
    /// (`grep -e --pattern`).
    pub switches: &'static [&'static str],
    /// Where the words of a command they run begin: from there on, every
    /// word is a digest, a flag's too, since it is no flag of theirs.
    pub runs: Runs,
    /// Whether their options may follow their operands, each option taking
    /// one word at most, as clap's do (`cargo test x --locked`). When not,
    /// their words from the first that is no flag, no `+…` and no known
    /// subcommand kept at its position (an operand, or a value an option
    /// may take) are all digests, a flag's too: a parser that stops at its
    /// first operand (`git config --get-regexp 'x.*' --pattern`) or an
    /// option of more values (`git update-index --cacheinfo`) takes what
    /// follows as values. When they may, their words from an unknown
    /// subcommand (`cargo foo`, whose own parser is unknown) are.
    pub options_after_operands: bool,
    /// Whether their first word, when it starts with `+`, picks their
    /// toolchain (`cargo +nightly test`): it is kept as a `plus` and takes
    /// no subcommand's position. Anywhere else, and for any other program,
    /// a word that starts with `+` is an operand (`grep +pattern file`).
    pub toolchain: bool,
}

/// Where the words of a command a program runs begin ([`SubcommandList`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// It runs no command its words name.
    Nothing,
    /// At its first word that is no flag after one of these paths of its
    /// words that are no flags: a path of one word is its first such word
    /// (`cargo run --release x …`, `rustup run stable …`), one of two is
    /// such a word right after the first (`git bisect run x …`,
    /// `git -C dir submodule foreach x …`).
    After(&'static [&'static str]),
    /// At its first word that is no flag: a command (`timeout 60 …`,
    /// `xargs -n1 …`, `env FOO=1 …`), a shell's script (`sh x.sh …`) or a
    /// path to search (`find . -exec …`).
    FirstOperand,
}

impl SubcommandList {
    /// Programs that run no command their words name.
    pub const fn of(
        programs: &'static [&'static str],
        subcommands: &'static [&'static str],
        switches: &'static [&'static str],
    ) -> Self {
        Self {
            programs,
            subcommands,
            switches,
            runs: Runs::Nothing,
            options_after_operands: false,
            toolchain: false,
        }
    }

    /// The same, their options read after their operands too, one word
    /// each, as clap reads them ([`Self::options_after_operands`]).
    pub const fn clap(self) -> Self {
        Self {
            options_after_operands: true,
            ..self
        }
    }

    /// Whether the option `flag` (its `=` and value cut) takes no value:
    /// a long one these switches name, or short letters each of which they
    /// name (`-uu`, `-la`).
    fn switch(&self, flag: &str) -> bool {
        match flag.strip_prefix("--") {
            Some(_) => self.switches.contains(&flag),
            None => flag.chars().skip(1).all(|letter| {
                let mut short = [0u8; 8];
                let short = format!("-{}", letter.encode_utf8(&mut short));
                self.switches.contains(&short.as_str())
            }),
        }
    }
}

/// The programs the runtime knows whatever the repository (ADR-t2268-1
/// decision 2, the runtime's common defaults) and their known subcommands:
/// git, dagq and the shell's common tools. A tool's own come from its
/// profile once declared
/// ([`super::toolchain::ToolchainProfile::subcommands`]).
const COMMON_SUBCOMMANDS: &[SubcommandList] = &[
    SubcommandList {
        programs: &["git"],
        subcommands: &[
            "add",
            "am",
            "apply",
            "bisect",
            "blame",
            "branch",
            "cat-file",
            "checkout",
            "cherry-pick",
            "clean",
            "clone",
            "commit",
            "config",
            "describe",
            "diff",
            "drop",
            "fetch",
            "for-each-ref",
            "format-patch",
            "fsck",
            "gc",
            "grep",
            "init",
            "list",
            "lock",
            "log",
            "ls-files",
            "ls-remote",
            "maintenance",
            "merge",
            "merge-base",
            "move",
            "mv",
            "notes",
            "pop",
            "prune",
            "pull",
            "push",
            "range-diff",
            "rebase",
            "reflog",
            "remote",
            "remove",
            "repair",
            "reset",
            "restore",
            "rev-list",
            "rev-parse",
            "revert",
            "rm",
            "save",
            "shortlog",
            "show",
            "show-ref",
            "sparse-checkout",
            "stash",
            "status",
            "submodule",
            "switch",
            "symbolic-ref",
            "tag",
            "unlock",
            "update",
            "update-ref",
            "worktree",
        ],
        switches: &[
            "-a",
            "-d",
            "-D",
            "-h",
            "-i",
            "-q",
            "-r",
            "-v",
            "-z",
            "--abort",
            "--all",
            "--amend",
            "--autosquash",
            "--autostash",
            "--binary",
            "--cached",
            "--check",
            "--color",
            "--continue",
            "--decorate",
            "--delete",
            "--dry-run",
            "--exit-code",
            "--ff",
            "--ff-only",
            "--first-parent",
            "--follow",
            "--force",
            "--force-with-lease",
            "--graph",
            "--hard",
            "--help",
            "--ignored",
            "--init",
            "--interactive",
            "--is-inside-work-tree",
            "--keep-index",
            "--list",
            "--merges",
            "--mixed",
            "--name-only",
            "--name-status",
            "--no-color",
            "--no-edit",
            "--no-ff",
            "--no-merges",
            "--no-pager",
            "--no-rebase",
            "--no-renames",
            "--no-verify",
            "--numstat",
            "--oneline",
            "--patch",
            "--porcelain",
            "--prune",
            "--quiet",
            "--quit",
            "--raw",
            "--rebase",
            "--recursive",
            "--reverse",
            "--set-upstream",
            "--short",
            "--shortstat",
            "--show-current",
            "--show-toplevel",
            "--signoff",
            "--soft",
            "--staged",
            "--stat",
            "--tags",
            "--verbose",
            "--verify",
            "--version",
        ],
        runs: Runs::After(&["bisect run", "submodule foreach"]),
        options_after_operands: false,
        toolchain: false,
    },
    // Its `plan`, which takes any words after it, is not one of them.
    SubcommandList::of(
        &["dagq"],
        &[
            "add",
            "agent",
            "answer",
            "ask",
            "asks",
            "cancel",
            "candidates",
            "ci",
            "close",
            "dependency",
            "dismiss",
            "doctor",
            "down",
            "draft",
            "edit",
            "eval",
            "events",
            "failures",
            "finding",
            "findings",
            "forecast",
            "goal",
            "graph",
            "help",
            "hold",
            "inbox",
            "init",
            "install",
            "integrate",
            "kpi",
            "lint",
            "list",
            "locate",
            "log",
            "mark",
            "marks",
            "migrate",
            "note",
            "notes",
            "observe",
            "planner",
            "planners",
            "proposal",
            "queue-broker",
            "ready",
            "rebind",
            "record",
            "recover",
            "related",
            "relay",
            "release",
            "replan",
            "replans",
            "report",
            "request",
            "requests",
            "resolve",
            "review",
            "revisit",
            "run",
            "search",
            "service",
            "set-goal",
            "set-paths",
            "set-priority",
            "setup-review",
            "show",
            "start",
            "stats",
            "status",
            "stop",
            "submit",
            "supervise",
            "throughput-review",
            "timeline",
            "up",
            "watch",
        ],
        &[
            "-f",
            "-h",
            "-V",
            "--all",
            "--allow-breaking",
            "--apply",
            "--auto-update",
            "--background",
            "--bypass-review",
            "--check",
            "--clear",
            "--cross",
            "--daily",
            "--draft",
            "--dry-run",
            "--follow",
            "--force",
            "--forecast-snapshots",
            "--full",
            "--headless",
            "--help",
            "--history",
            "--ignore-deferrals",
            "--in-cmux",
            "--inherit",
            "--interactive",
            "--ledger",
            "--next",
            "--no-claude",
            "--no-evidence",
            "--no-paths",
            "--no-provider-fallback",
            "--no-push",
            "--no-tags",
            "--no-verify",
            "--no-wait",
            "--no-wait-for-build",
            "--none",
            "--observe-daily",
            "--once",
            "--open",
            "--plugin-only",
            "--queue",
            "--report-daily",
            "--rerun",
            "--resume",
            "--rollback",
            "--skip-e2e",
            "--switchable",
            "--until-attention",
            "--version",
            "--wait",
            "--wait-for-build",
            "--yes",
        ],
    )
    .clap(),
    // Programs kept by name, none of whose words is a subcommand. A
    // program whose arguments are text to print or compare (`echo`,
    // `printf`, `test`, `[`, `[[`), one whose option takes two values
    // (`jq --arg NAME VALUE`) or one whose option runs a command (`fd -x`)
    // is not one of them.
    SubcommandList::of(&["awk"], &[], &["--posix", "--help", "--version"]),
    SubcommandList::of(
        &["cat"],
        &[],
        &[
            "-A",
            "-b",
            "-e",
            "-E",
            "-n",
            "-s",
            "-t",
            "-T",
            "-u",
            "-v",
            "--number",
            "--show-all",
            "--squeeze-blank",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["cd"], &[], &["-@", "-e", "-L", "-P"]),
    SubcommandList::of(
        &["chmod"],
        &[],
        &[
            "-c",
            "-f",
            "-R",
            "-v",
            "--changes",
            "--quiet",
            "--recursive",
            "--silent",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["cp"],
        &[],
        &[
            "-a",
            "-b",
            "-d",
            "-f",
            "-H",
            "-i",
            "-l",
            "-L",
            "-n",
            "-p",
            "-P",
            "-r",
            "-R",
            "-s",
            "-T",
            "-u",
            "-v",
            "-x",
            "--archive",
            "--force",
            "--interactive",
            "--no-clobber",
            "--parents",
            "--recursive",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["cut"],
        &[],
        &[
            "-n",
            "-s",
            "-z",
            "--complement",
            "--only-delimited",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["date"],
        &[],
        &[
            "-R",
            "-u",
            "--rfc-email",
            "--universal",
            "--utc",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["diff"],
        &[],
        &[
            "-a",
            "-b",
            "-B",
            "-c",
            "-d",
            "-e",
            "-E",
            "-H",
            "-i",
            "-n",
            "-N",
            "-p",
            "-q",
            "-r",
            "-s",
            "-t",
            "-T",
            "-u",
            "-w",
            "-y",
            "--brief",
            "--color",
            "--ignore-all-space",
            "--ignore-case",
            "--new-file",
            "--recursive",
            "--report-identical-files",
            "--side-by-side",
            "--strip-trailing-cr",
            "--text",
            "--unified",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["false"], &[], &[]),
    SubcommandList::of(
        &["grep"],
        &[],
        &[
            "-a",
            "-c",
            "-E",
            "-F",
            "-G",
            "-h",
            "-H",
            "-i",
            "-I",
            "-l",
            "-L",
            "-n",
            "-o",
            "-P",
            "-q",
            "-r",
            "-R",
            "-s",
            "-T",
            "-U",
            "-v",
            "-V",
            "-w",
            "-x",
            "-y",
            "-z",
            "-Z",
            "--color",
            "--colour",
            "--count",
            "--extended-regexp",
            "--files-with-matches",
            "--files-without-match",
            "--fixed-strings",
            "--ignore-case",
            "--invert-match",
            "--line-number",
            "--line-regexp",
            "--no-filename",
            "--no-messages",
            "--null",
            "--only-matching",
            "--perl-regexp",
            "--quiet",
            "--recursive",
            "--silent",
            "--with-filename",
            "--word-regexp",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["gzip"],
        &[],
        &[
            "-1",
            "-9",
            "-c",
            "-d",
            "-f",
            "-k",
            "-l",
            "-n",
            "-N",
            "-q",
            "-r",
            "-t",
            "-v",
            "--best",
            "--decompress",
            "--fast",
            "--force",
            "--keep",
            "--list",
            "--quiet",
            "--recursive",
            "--stdout",
            "--test",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["head"],
        &[],
        &[
            "-q",
            "-v",
            "-z",
            "--quiet",
            "--silent",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["ln"],
        &[],
        &[
            "-f",
            "-i",
            "-L",
            "-n",
            "-P",
            "-r",
            "-s",
            "-T",
            "-v",
            "--force",
            "--no-dereference",
            "--relative",
            "--symbolic",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["ls"],
        &[],
        &[
            "-1",
            "-a",
            "-A",
            "-B",
            "-c",
            "-C",
            "-d",
            "-f",
            "-F",
            "-g",
            "-G",
            "-h",
            "-H",
            "-i",
            "-k",
            "-l",
            "-L",
            "-m",
            "-n",
            "-N",
            "-o",
            "-p",
            "-q",
            "-Q",
            "-r",
            "-R",
            "-s",
            "-S",
            "-t",
            "-u",
            "-U",
            "-v",
            "-x",
            "-X",
            "-Z",
            "--all",
            "--almost-all",
            "--color",
            "--human-readable",
            "--recursive",
            "--reverse",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["make"],
        &[],
        &[
            "-B",
            "-d",
            "-e",
            "-i",
            "-k",
            "-n",
            "-q",
            "-r",
            "-R",
            "-s",
            "-S",
            "-t",
            "-w",
            "--always-make",
            "--dry-run",
            "--just-print",
            "--keep-going",
            "--no-print-directory",
            "--print-directory",
            "--quiet",
            "--silent",
            "--touch",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["mkdir"],
        &[],
        &["-p", "-v", "--parents", "--verbose", "--help", "--version"],
    ),
    SubcommandList::of(
        &["mv"],
        &[],
        &[
            "-f",
            "-i",
            "-n",
            "-T",
            "-u",
            "-v",
            "--force",
            "--interactive",
            "--no-clobber",
            "--update",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["pwd"], &[], &["-L", "-P"]),
    SubcommandList::of(
        &["rg"],
        &[],
        &[
            "-.",
            "-0",
            "-a",
            "-b",
            "-c",
            "-F",
            "-H",
            "-i",
            "-I",
            "-l",
            "-L",
            "-n",
            "-N",
            "-o",
            "-p",
            "-P",
            "-q",
            "-s",
            "-S",
            "-u",
            "-U",
            "-v",
            "-w",
            "-x",
            "-z",
            "--case-sensitive",
            "--count",
            "--files",
            "--files-with-matches",
            "--files-without-match",
            "--fixed-strings",
            "--follow",
            "--heading",
            "--hidden",
            "--ignore-case",
            "--invert-match",
            "--json",
            "--line-number",
            "--line-regexp",
            "--multiline",
            "--no-filename",
            "--no-heading",
            "--no-ignore",
            "--no-line-number",
            "--null",
            "--only-matching",
            "--pcre2",
            "--quiet",
            "--smart-case",
            "--stats",
            "--text",
            "--vimgrep",
            "--with-filename",
            "--word-regexp",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["rm"],
        &[],
        &[
            "-d",
            "-f",
            "-i",
            "-I",
            "-r",
            "-R",
            "-v",
            "--dir",
            "--force",
            "--interactive",
            "--recursive",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["sed"],
        &[],
        &[
            "-E",
            "-n",
            "-r",
            "-s",
            "-u",
            "-z",
            "--debug",
            "--null-data",
            "--posix",
            "--quiet",
            "--regexp-extended",
            "--separate",
            "--silent",
            "--unbuffered",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["sleep"], &[], &["--help", "--version"]),
    SubcommandList::of(
        &["sort"],
        &[],
        &[
            "-b",
            "-c",
            "-C",
            "-d",
            "-f",
            "-g",
            "-h",
            "-i",
            "-m",
            "-M",
            "-n",
            "-r",
            "-R",
            "-s",
            "-u",
            "-V",
            "-z",
            "--human-numeric-sort",
            "--ignore-case",
            "--numeric-sort",
            "--reverse",
            "--stable",
            "--unique",
            "--version-sort",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["tail"],
        &[],
        &[
            "-f",
            "-F",
            "-q",
            "-r",
            "-v",
            "-z",
            "--follow",
            "--quiet",
            "--retry",
            "--silent",
            "--verbose",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["tar"],
        &[],
        &[
            "-c",
            "-h",
            "-j",
            "-J",
            "-k",
            "-O",
            "-p",
            "-P",
            "-t",
            "-v",
            "-x",
            "-z",
            "--bzip2",
            "--create",
            "--extract",
            "--gzip",
            "--keep-old-files",
            "--list",
            "--to-stdout",
            "--verbose",
            "--xz",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["tee"],
        &[],
        &[
            "-a",
            "-i",
            "-p",
            "--append",
            "--ignore-interrupts",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["touch"],
        &[],
        &[
            "-a",
            "-c",
            "-h",
            "-m",
            "--no-create",
            "--no-dereference",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["tr"],
        &[],
        &[
            "-c",
            "-C",
            "-d",
            "-s",
            "-t",
            "--complement",
            "--delete",
            "--squeeze-repeats",
            "--truncate-set1",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["true"], &[], &[]),
    SubcommandList::of(
        &["uniq"],
        &[],
        &[
            "-c",
            "-d",
            "-D",
            "-i",
            "-u",
            "-z",
            "--count",
            "--ignore-case",
            "--repeated",
            "--unique",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(
        &["wc"],
        &[],
        &[
            "-c",
            "-l",
            "-L",
            "-m",
            "-w",
            "--bytes",
            "--chars",
            "--lines",
            "--max-line-length",
            "--words",
            "--help",
            "--version",
        ],
    ),
    SubcommandList::of(&["which"], &[], &["-a", "-s"]),
    // Those that run a command, a shell's script or what they find.
    SubcommandList {
        programs: &["bash", "dash", "ksh", "sh", "zsh"],
        subcommands: &[],
        switches: &[
            "-a",
            "-b",
            "-B",
            "-C",
            "-e",
            "-E",
            "-f",
            "-h",
            "-H",
            "-i",
            "-k",
            "-l",
            "-m",
            "-n",
            "-p",
            "-P",
            "-s",
            "-t",
            "-T",
            "-u",
            "-v",
            "-x",
            "--login",
            "--noprofile",
            "--norc",
            "--posix",
            "--restricted",
            "--verbose",
            "--help",
            "--version",
        ],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["env"],
        subcommands: &[],
        switches: &[
            "-0",
            "-i",
            "-v",
            "--debug",
            "--ignore-environment",
            "--null",
            "--help",
            "--version",
        ],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["find"],
        subcommands: &[],
        switches: &["-H", "-L", "-P"],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["nice"],
        subcommands: &[],
        switches: &["--help", "--version"],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["nohup"],
        subcommands: &[],
        switches: &["--help", "--version"],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["timeout"],
        subcommands: &[],
        switches: &[
            "-v",
            "--foreground",
            "--preserve-status",
            "--verbose",
            "--help",
            "--version",
        ],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
    SubcommandList {
        programs: &["xargs"],
        subcommands: &[],
        switches: &[
            "-0",
            "-o",
            "-p",
            "-r",
            "-t",
            "-x",
            "--exit",
            "--interactive",
            "--no-run-if-empty",
            "--null",
            "--open-tty",
            "--verbose",
            "--help",
            "--version",
        ],
        runs: Runs::FirstOperand,
        options_after_operands: false,
        toolchain: false,
    },
];

/// The programs a shape keeps by name and the known subcommands it keeps
/// in plain (ADR-t1673-1): the runtime's common ones, and those of the
/// profiles a repository declares ([`Self::of`]). A program none names is
/// its digest, with every word after it.
///
/// It carries the queue's [`DigestKey`] too ([`Self::keyed`]): without one,
/// no command is shaped.
#[derive(Debug, Clone)]
pub struct Subcommands {
    lists: Vec<SubcommandList>,
    key: Option<DigestKey>,
}

impl Default for Subcommands {
    fn default() -> Self {
        Self::common()
    }
}

impl Subcommands {
    /// The runtime's common known subcommands alone, as for a repository
    /// that declares no profile.
    pub fn common() -> Self {
        Self {
            lists: COMMON_SUBCOMMANDS.to_vec(),
            key: None,
        }
    }

    /// The common ones and those of each profile `toolchains` declares.
    pub fn of(toolchains: &super::toolchain::Toolchains) -> Self {
        let mut lists = COMMON_SUBCOMMANDS.to_vec();
        for profile in toolchains.profiles() {
            lists.extend_from_slice(profile.subcommands());
        }
        Self { lists, key: None }
    }

    /// The same, the words digested under `key`, the queue's; with none, no
    /// command is shaped.
    #[must_use]
    pub fn keyed(mut self, key: Option<DigestKey>) -> Self {
        self.key = key;
        self
    }

    /// The digest of `word` under the key; empty without one, which
    /// [`shape`] never digests with.
    fn digest(&self, word: &str) -> String {
        self.key
            .as_ref()
            .map_or_else(String::new, |key| key.digest(word))
    }

    /// Those of a repository that declares the Rust profile, as this one
    /// does.
    #[cfg(test)]
    pub fn rust() -> Self {
        use super::toolchain::{BUILTIN, ToolchainDeclaration, Toolchains};
        let declared = Toolchains::resolve(
            &BUILTIN,
            &[ToolchainDeclaration {
                name: "rust".to_owned(),
                profile: "rust".to_owned(),
                paths: Vec::new(),
                line: 1,
            }],
        )
        .unwrap();
        Self::of(&declared).keyed(Some(DigestKey::test()))
    }

    /// The list that names the program named `name`; `None` for a program
    /// no list names.
    fn of_program(&self, name: &str) -> Option<&SubcommandList> {
        self.lists.iter().find(|list| list.programs.contains(&name))
    }
}

/// Whether `text` may be a subcommand: `^[a-z][a-z0-9-]{0,23}$`.
fn subcommand(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && text.len() <= 24
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The shaped parts of one part's `words`: one, or the parts of a shell's
/// script.
fn shape_part(words: &[Word], depth: usize, known: &Subcommands) -> Vec<ShapePart> {
    // A part that runs no program keeps its words' digests alone.
    if words.first().is_some_and(|word| word.pattern) {
        return vec![ShapePart {
            args: words
                .iter()
                .map(|word| ShapeArg::Word {
                    word: known.digest(&word.text),
                })
                .collect(),
            ..ShapePart::default()
        }];
    }
    let mut part = ShapePart::default();
    let mut rest = words;
    let mut looped = false;
    while let Some((word, after)) = rest.split_first() {
        if word.quoted || !RESERVED.contains(&word.text.as_str()) {
            break;
        }
        part.lead.push(word.text.clone());
        rest = after;
        if matches!(word.text.as_str(), "for" | "select" | "case") {
            looped = true;
            break;
        }
    }
    if looped {
        // `for NAME in …`: its variable is assigned, its list is words.
        if part.lead.last().is_some_and(|lead| lead != "case")
            && let Some((name, after)) = rest.split_first()
        {
            // Kept only when the whole word is a variable's name.
            part.assigns.push(
                if !name.quoted && assigned(&format!("{}=", name.text)) == Some(name.text.as_str())
                {
                    name.text.clone()
                } else {
                    known.digest(&name.text)
                },
            );
            rest = after;
        }
        part.args = rest
            .iter()
            .map(|word| ShapeArg::Word {
                word: known.digest(&word.text),
            })
            .collect();
        return vec![part];
    }
    // A word whose name is quoted (`"NAME=x"`) assigns nothing: it is the
    // program.
    while let Some((word, after)) = rest.split_first() {
        let Some(name) = assigned(&word.text).filter(|_| !word.name_quoted) else {
            break;
        };
        part.assigns.push(name.to_owned());
        rest = after;
    }
    let Some((program, args)) = rest.split_first() else {
        return if part.lead.is_empty() && part.assigns.is_empty() {
            Vec::new()
        } else {
            vec![part]
        };
    };
    let name = program
        .text
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(&program.text)
        .to_owned();
    if depth < MAX_SCRIPT_DEPTH
        && SHELLS.contains(&name.as_str())
        && let Some(script) = shell_script(args)
    {
        let mut parts = parts_of(script, depth + 1, known);
        if let Some(first) = parts.first_mut() {
            let mut lead = std::mem::take(&mut part.lead);
            lead.append(&mut first.lead);
            first.lead = lead;
            let mut assigns = std::mem::take(&mut part.assigns);
            assigns.append(&mut first.assigns);
            first.assigns = assigns;
        }
        return parts;
    }
    // Only a program a list names keeps its name, and its flags' names and
    // its known subcommands; any other first word (an unknown program, a
    // substitution, a quoted text, or a word a misread split put there) is
    // its digest, and so is every word after it.
    match known.of_program(&name) {
        Some(list) => {
            part.program = Some(name);
            part.args = shape_args(list, args, known);
        }
        None => {
            part.program = Some(known.digest(&program.text));
            part.args = args
                .iter()
                .map(|word| ShapeArg::Word {
                    word: known.digest(&word.text),
                })
                .collect();
        }
    }
    vec![part]
}

/// The script of a shell's `-c` (`bash -lc 'script'`): the word after a
/// flag of single letters with `c`, every word before it a flag.
fn shell_script(args: &[Word]) -> Option<&str> {
    for (at, word) in args.iter().enumerate() {
        let flag = word.text.strip_prefix('-')?;
        if flag.starts_with('-') {
            return None;
        }
        if flag.contains('c') {
            return args.get(at + 1).map(|script| script.text.as_str());
        }
    }
    None
}

/// The arguments of a part after its program, a program `list` names.
fn shape_args(list: &SubcommandList, words: &[Word], keyed: &Subcommands) -> Vec<ShapeArg> {
    let known = list.subcommands;
    let mut args = Vec::new();
    let mut flagged = false;
    let mut positions = 0;
    let mut after_secret = false;
    let mut after_dashdash: Option<usize> = None;
    // The words of a command it runs began: none is its own. Armed, the
    // next word that is no flag begins them ([`Runs`]).
    let mut running = false;
    let mut armed = list.runs == Runs::FirstOperand;
    // The words that are no flags and no option's possible value.
    let mut operands = 0usize;
    let mut previous: Option<&str> = None;
    // The word before is an option that may take this one as its value.
    let mut takes_next = false;
    for (at, word) in words.iter().enumerate() {
        let text = word.text.as_str();
        if running {
            args.push(ShapeArg::Word {
                word: keyed.digest(text),
            });
            continue;
        }
        if let Some(count) = after_dashdash.as_mut() {
            *count += 1;
            continue;
        }
        if text == "--" {
            after_dashdash = Some(0);
            continue;
        }
        let value = std::mem::take(&mut takes_next);
        if text.len() > 1 && text.starts_with('-') {
            flagged = true;
            // An option's value that looks like a flag (`grep -e
            // --pattern`) is a digest.
            if value {
                args.push(ShapeArg::Word {
                    word: keyed.digest(text),
                });
                continue;
            }
            let (name, value) = match text.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (text, None),
            };
            // Only a word whose name is unquoted and looks like a flag's
            // keeps it; any other (a quoted text that starts with `-`) is a
            // word. A short flag keeps its letter, and what is glued to it
            // (`-pS3cr3t`, `-j8`) is its value. Unless the option takes no
            // value (a switch) or its value is attached by `=`, the next
            // word may be its value, its name quoted too (`'-e'` is `-e`).
            let flag = !word.name_quoted && flag_name(name);
            takes_next = value.is_none() && !list.switch(name);
            args.push(if !flag {
                ShapeArg::Word {
                    word: keyed.digest(text),
                }
            } else if !name.starts_with("--") && text.len() > 2 {
                ShapeArg::Flag {
                    flag: text[..2].to_owned(),
                    value: Some(keyed.digest(&text[2..])),
                }
            } else {
                ShapeArg::Flag {
                    flag: name.to_owned(),
                    value: value.map(|value| keyed.digest(value)),
                }
            });
            continue;
        }
        // Only the first word of a program that picks its toolchain by it
        // is one; any other `+…` is an operand.
        if list.toolchain && at == 0 && text.len() > 1 && text.starts_with('+') {
            args.push(ShapeArg::Plus {
                plus: keyed.digest(text),
            });
            continue;
        }
        let position = !flagged && positions < SUB_POSITIONS;
        if position {
            positions += 1;
        }
        running = armed;
        // The word after one a secret may follow (`login`, `AUTH`) is no
        // subcommand whatever it looks like.
        let sub =
            !running && position && !after_secret && subcommand(text) && known.contains(&text);
        after_secret = SECRET_SUBS.contains(&text.to_ascii_lowercase().as_str());
        // A path is looked for at an option's possible value too
        // (`cargo --config run x` may run `x`); a one-word path is the
        // first word that is no flag's possible value (`cargo --color
        // never run x`).
        if let Runs::After(paths) = list.runs {
            armed = paths.iter().any(|path| match path.split_once(' ') {
                Some((first, last)) => last == text && previous == Some(first),
                None => *path == text && operands == 0,
            });
        }
        // Past an operand (or a value an option may take) of a parser
        // that may stop at it, or an unknown subcommand of one that may
        // not, every word is a digest ([`SubcommandList::options_after_operands`]).
        if !sub && (!list.options_after_operands || (!value && operands == 0)) {
            running = true;
        }
        if !value {
            operands += 1;
        }
        previous = Some(text);
        args.push(if sub {
            ShapeArg::Sub {
                sub: text.to_owned(),
            }
        } else {
            ShapeArg::Word {
                word: keyed.digest(text),
            }
        });
    }
    if let Some(count) = after_dashdash {
        args.push(ShapeArg::AfterDashdash {
            after_dashdash: count,
        });
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(word: &str) -> String {
        DigestKey::test().digest(word)
    }

    fn rust() -> Subcommands {
        Subcommands::rust()
    }

    fn value(command: &str) -> serde_json::Value {
        shape_value_with(command, &rust()).unwrap()
    }

    /// A program is kept by name with its subcommands only once its profile
    /// is declared (ADR-t2268-1 decision 3): without it, cargo and every
    /// word after it are digests, and git's and dagq's, the runtime's
    /// common ones, stay plain.
    #[test]
    fn a_tools_subcommands_are_plain_only_under_its_declared_profile() {
        let none = Subcommands::of(&crate::domain::toolchain::Toolchains::none())
            .keyed(Some(DigestKey::test()));
        assert_eq!(
            shape_value_with("cargo test --locked", &none).unwrap(),
            json!({"parts": [{"program": d("cargo"), "args": [{"word": d("test")}, {"word": d("--locked")}]}]})
        );
        assert_eq!(
            shape_value_with(
                "cargo test",
                &Subcommands::common().keyed(Some(DigestKey::test()))
            ),
            shape_value_with("cargo test", &none)
        );
        assert_eq!(
            value("cargo test --locked")["parts"][0]["args"][0],
            json!({"sub": "test"})
        );
        for known in [none, rust()] {
            assert_eq!(
                shape_value_with("git status", &known).unwrap()["parts"][0]["args"],
                json!([{"sub": "status"}])
            );
            assert_eq!(
                shape_value_with("dagq run log", &known).unwrap()["parts"][0]["args"],
                json!([{"sub": "run"}, {"sub": "log"}])
            );
        }
    }

    /// A digest is the first sixteen hex of the HMAC-SHA256 under the
    /// queue's key of the versioned word (ADR-t1673-1 decision 10): one
    /// word is one digest under one key, another key's differs, and
    /// neither is the word's plain hash, which a dictionary would match.
    /// Without a key, nothing is shaped.
    #[test]
    fn a_digest_is_the_first_sixteen_hex_of_the_versioned_hmac_under_the_queues_key() {
        // RFC 4231, test case 2.
        let mac: String = hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let key = DigestKey::test();
        let full: String = hmac_sha256(&[7; DIGEST_KEY_BYTES], &[b"dagq-cmd-v2\0it"])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(key.digest("it"), full[..16]);
        assert_eq!(d("it").len(), 16);
        assert_eq!(key.digest("hunter2"), DigestKey::test().digest("hunter2"));
        assert_ne!(d("it"), d("plugin"));
        let other = DigestKey::from_bytes(&[8; DIGEST_KEY_BYTES]).unwrap();
        assert_ne!(other.digest("hunter2"), key.digest("hunter2"));
        let plain: String = Sha256::digest(b"dagq-cmd-v2\0hunter2")
            .iter()
            .take(DIGEST_HEX / 2)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_ne!(key.digest("hunter2"), plain);
        assert_eq!(DigestKey::from_bytes(&[7; 31]), None);
        assert_eq!(format!("{key:?}"), "DigestKey(..)");
        // The same command is one shape in one queue, another in another.
        assert_eq!(value("git log hunter2"), value("git log hunter2"));
        let elsewhere = Subcommands::rust().keyed(Some(other));
        assert_ne!(
            shape_value_with("git log hunter2", &elsewhere),
            Some(value("git log hunter2"))
        );
        assert_eq!(
            shape_value_with("git log hunter2", &Subcommands::common()),
            None
        );
    }

    /// The commands of the representative table (the left column of task
    /// 1674's): each one's shape, and those the classification tells apart
    /// differ.
    #[test]
    fn the_representative_commands_take_their_shapes() {
        let cases = [
            (
                "cargo test --locked --test it",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "test"}, {"flag": "--locked"}, {"flag": "--test"}, {"word": d("it")}]}]}),
            ),
            (
                "cargo test --locked --test plugin",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "test"}, {"flag": "--locked"}, {"flag": "--test"}, {"word": d("plugin")}]}]}),
            ),
            (
                "cargo test --locked --test it runtime_claim::",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "test"}, {"flag": "--locked"}, {"flag": "--test"}, {"word": d("it")},
                    {"word": d("runtime_claim::")}]}]}),
            ),
            (
                "cargo test --locked --test e2e -- --ignored",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "test"}, {"flag": "--locked"}, {"flag": "--test"}, {"word": d("e2e")},
                    {"after_dashdash": 1}]}]}),
            ),
            (
                "cargo llvm-cov nextest --locked --workspace --fail-under-lines 80",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "llvm-cov"}, {"sub": "nextest"}, {"flag": "--locked"},
                    {"flag": "--workspace"}, {"flag": "--fail-under-lines"}, {"word": d("80")}]}]}),
            ),
            (
                "cargo +nightly fmt --all --check",
                json!({"parts": [{"program": "cargo", "args": [
                    {"plus": d("+nightly")}, {"sub": "fmt"}, {"flag": "--all"}, {"flag": "--check"}]}]}),
            ),
            (
                "cargo nextest run --locked --test it --stress-count 5 -E 'test(=a::b)'",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "nextest"}, {"sub": "run"}, {"flag": "--locked"}, {"flag": "--test"},
                    {"word": d("it")}, {"flag": "--stress-count"}, {"word": d("5")},
                    {"flag": "-E"}, {"word": d("test(=a::b)")}]}]}),
            ),
            (
                "sh scripts/cargo-brief.sh cargo clippy --locked --all-targets -- -D warnings",
                json!({"parts": [{"program": "sh", "args": [
                    {"word": d("scripts/cargo-brief.sh")}, {"word": d("cargo")}, {"word": d("clippy")},
                    {"word": d("--locked")}, {"word": d("--all-targets")}, {"word": d("--")},
                    {"word": d("-D")}, {"word": d("warnings")}]}]}),
            ),
            (
                "/usr/local/bin/git status --short",
                json!({"parts": [{"program": "git", "args": [{"sub": "status"}, {"flag": "--short"}]}]}),
            ),
            (
                "cargo build --message-format=json --release",
                json!({"parts": [{"program": "cargo", "args": [
                    {"sub": "build"}, {"flag": "--message-format", "value": d("json")},
                    {"flag": "--release"}]}]}),
            ),
            (
                "RUST_LOG=debug CARGO_TARGET_DIR=$TMPDIR/t cargo test 2>&1 | tail -n 50 > out.txt",
                json!({"parts": [
                    {"assigns": ["RUST_LOG", "CARGO_TARGET_DIR"], "program": "cargo", "args": [{"sub": "test"}]},
                    {"program": "tail", "args": [{"flag": "-n"}, {"word": d("50")}]}]}),
            ),
            (
                "until [ -f done ]; do sleep 5; done && dagq status || echo failed; true",
                json!({"parts": [
                    {"lead": ["until"], "program": d("["), "args": [{"word": d("-f")}, {"word": d("done")}, {"word": d("]")}]},
                    {"lead": ["do"], "program": "sleep", "args": [{"word": d("5")}]},
                    {"lead": ["done"], "args": []},
                    {"program": "dagq", "args": [{"sub": "status"}]},
                    {"program": d("echo"), "args": [{"word": d("failed")}]},
                    {"program": "true", "args": []}]}),
            ),
            (
                "for f in a b; do cargo test $f; done",
                json!({"parts": [
                    {"lead": ["for"], "assigns": ["f"], "args": [{"word": d("in")}, {"word": d("a")}, {"word": d("b")}]},
                    {"lead": ["do"], "program": "cargo", "args": [{"sub": "test"}, {"word": d("$f")}]},
                    {"lead": ["done"], "args": []}]}),
            ),
        ];
        for (command, expected) in &cases {
            assert_eq!(value(command), *expected, "{command}");
        }
        // What the classification tells apart takes different shapes.
        let shapes: Vec<_> = [
            "cargo test --locked --test it",
            "cargo test --locked --test plugin",
            "cargo test --locked --test it runtime_claim::",
        ]
        .into_iter()
        .map(value)
        .collect();
        assert_ne!(shapes[0], shapes[1]);
        assert_ne!(shapes[0], shapes[2]);
        assert_ne!(shapes[1], shapes[2]);
    }

    /// No flag's value, no filter outside a subcommand's position and no
    /// command text is in a shape: only names, subcommands and digests.
    #[test]
    fn no_value_filter_or_command_text_is_in_a_shape() {
        let command = "cargo test --locked --test it runtime_claim::a_claim --features=secret-feature -- --nocapture SECRET_FILTER";
        let text = value(command).to_string();
        for kept_out in [
            "runtime_claim",
            "a_claim",
            "secret-feature",
            "SECRET_FILTER",
            "--nocapture",
            "\"it\"",
            command,
        ] {
            assert!(!text.contains(kept_out), "{kept_out} in {text}");
        }
        assert!(text.contains("--features"), "{text}");
        assert!(text.contains(&d("secret-feature")), "{text}");
    }

    /// A value quoted on its own keeps its flag's name and its digest; a
    /// value glued to a short flag is that flag's value; an array's
    /// assignment keeps its name alone.
    #[test]
    fn quoted_and_glued_values_and_arrays_keep_only_names_and_digests() {
        assert_eq!(
            value("cargo test --test=\"it\" --features='a b'")["parts"][0]["args"],
            json!([
                {"sub": "test"},
                {"flag": "--test", "value": d("it")},
                {"flag": "--features", "value": d("a b")},
            ])
        );
        assert_eq!(
            value("cargo test --test=\"it\""),
            value("cargo test --test=it")
        );
        // A quoted name is no flag's.
        assert_eq!(
            value("cargo test \"--test=it\"")["parts"][0]["args"][1],
            json!({"word": d("--test=it")})
        );
        // What follows a short flag with letters glued to it may be its
        // value: `-u` after `-pS3cr3t` is a digest.
        let shape = value("cargo build -pS3cr3t -u root -j8");
        assert_eq!(
            shape["parts"][0]["args"],
            json!([
                {"sub": "build"},
                {"flag": "-p", "value": d("S3cr3t")},
                {"word": d("-u")},
                {"word": d("root")},
                {"flag": "-j", "value": d("8")},
            ])
        );
        let shape = value("TOKENS=('topsecret123' x) cargo build; A=(b) ");
        let text = shape.to_string();
        assert!(!text.contains("topsecret123"), "{text}");
        assert_eq!(
            shape,
            json!({"parts": [
                {"assigns": ["TOKENS"], "program": "cargo", "args": [{"sub": "build"}]},
                {"assigns": ["A"], "args": []},
            ]})
        );
    }

    /// A word next to a parenthesis is never a program kept in plain: a
    /// process substitution and an extglob are words, a `case` pattern
    /// (its alternatives, after `do` or `then`, in a nested `case`) and
    /// what follows a subshell's close run no program, and a subshell's
    /// command, an arm's and a function's body's are still their programs.
    #[test]
    fn words_by_parentheses_keep_no_text() {
        for (command, secret) in [
            ("cat <(echo) hunter2", "hunter2"),
            ("diff -u <(sort a) secret.env", "secret.env"),
            ("tee >(gzip > out) s3cr3t", "s3cr3t"),
            ("rm -rf !(hunter2)", "hunter2"),
            ("case $x in a) echo;; s3cr3t) echo;; esac", "s3cr3t"),
            ("case $x in\n  s3cr3t)\n    echo\n    ;;\nesac", "s3cr3t"),
            ("case $x in (s3cr3t) echo;; esac", "s3cr3t"),
            ("(cd x) hunter2", "hunter2"),
            ("(cd x) (hunter2)", "hunter2"),
            ("((total = hunter2 + 1))", "hunter2"),
            ("case $1 in a) echo;; hunter2|b) echo;; esac", "hunter2"),
            ("case $x in\n  hunter2|b)\n echo;;\nesac", "hunter2"),
            ("case $x in (hunter2|b) echo;; esac", "hunter2"),
            (
                "for f in a b; do case $f in (hunter2) echo;; esac; done",
                "hunter2",
            ),
            (
                "if true; then case $f in hunter2) echo;; esac; fi",
                "hunter2",
            ),
            ("case $x in a) (cd x) hunter2;; esac", "hunter2"),
            ("case $x in a) echo;;& hunter2) echo;; esac", "hunter2"),
            (
                "case $x in a) case $y in b) echo;; esac;; hunter2) echo;; esac",
                "hunter2",
            ),
            ("case $x in\n hunter2\n) echo;; esac", "hunter2"),
        ] {
            let text = value(command).to_string();
            assert!(!text.contains(secret), "{secret} of {command} in {text}");
        }
        assert_eq!(
            value("(git status) && echo")["parts"],
            json!([
                {"program": "git", "args": [{"sub": "status"}]},
                {"program": d("echo"), "args": []},
            ])
        );
        for (command, at) in [
            ("case $x in a) git status;; esac", 2),
            ("case $x in a) (cd x && git status);; esac", 3),
            ("case $x in a) echo;; esac | git status", 4),
            ("( case $x in a) echo;; esac ) && git status", 4),
            ("f() { git status; }", 1),
            ("f () { git status; }", 1),
        ] {
            let parts = &value(command)["parts"];
            let part = parts[at].as_object().unwrap();
            assert_eq!(part["program"], "git", "{command}: {parts}");
            assert_eq!(
                part["args"],
                json!([{"sub": "status"}]),
                "{command}: {parts}"
            );
        }
    }

    /// Only a listed program's known subcommands are kept in plain
    /// (ADR-t1673-1): a value at a subcommand's position, a filter there,
    /// and a program no list names with every word after it, are their
    /// digests.
    #[test]
    fn only_a_programs_known_subcommands_are_kept_in_plain() {
        let shape = value("printf %s hunter2 > password.txt");
        assert_eq!(
            shape,
            json!({"parts": [{"program": d("printf"), "args": [{"word": d("%s")}, {"word": d("hunter2")}]}]})
        );
        for (command, secret) in [
            ("printf %s hunter2 > password.txt", "hunter2"),
            ("printf %s hunter2 > password.txt", "password"),
            ("echo hunter2", "hunter2"),
            ("redis-cli auth hunter2", "hunter2"),
            ("git stash push hunter2", "hunter2"),
            ("npm run deploy-secret", "deploy-secret"),
            ("cargo test runtime", "runtime"),
        ] {
            let text = value(command).to_string();
            assert!(!text.contains(secret), "{secret} of {command} in {text}");
        }
        assert_eq!(
            value("git stash push hunter2")["parts"][0]["args"],
            json!([{"sub": "stash"}, {"sub": "push"}, {"word": d("hunter2")}])
        );
        assert_eq!(
            value("npm run deploy-secret")["parts"][0],
            json!({"program": d("npm"), "args": [{"word": d("run")}, {"word": d("deploy-secret")}]})
        );
        // Known by the program's name without its path, through a shell's
        // script too.
        assert_eq!(
            value("bash -lc '/usr/bin/git worktree add x'")["parts"][0]["args"],
            json!([{"sub": "worktree"}, {"sub": "add"}, {"word": d("x")}])
        );
    }

    /// What a shape keeps in plain is only what a list names, however the
    /// command was split (ADR-t1673-1): a listed program's name, its known
    /// subcommands and its flags' names, the shell's reserved words, and
    /// the names of the variables assigned; every other word, a program no
    /// list names among them, is a digest. A word a misread split puts at
    /// a program's place (a conditional expression's value after its `||`,
    /// a `printf`'s argument) is no listed program, and so its digest.
    #[test]
    fn only_listed_words_are_plain_however_the_command_is_split() {
        let known = rust();
        let hex =
            |text: &str| text.len() == DIGEST_HEX && text.chars().all(|c| c.is_ascii_hexdigit());
        for command in [
            "printf %s hunter2 > password.txt",
            "[[ \"$value\" == ok || hunter2 == \"$value\" ]]",
            "[[ $a == b && hunter2 ]] && git status",
            "[[ $a =~ (hunter2|b) ]]",
            "[ \"$a\" = ok ] || hunter2 = \"$a\"",
            "test hunter2 = x",
            "echo --hunter2",
            "hunter2",
            "./bin/hunter2 --flag",
            "x=$(hunter2) cargo test",
            "cat <<EOF\nhunter2\nEOF",
            "case $x in hunter2) hunter2;; esac",
            "{ hunter2; }",
            "if hunter2; then hunter2; fi",
            "time hunter2 | ! hunter2",
            "exec hunter2; eval hunter2; env hunter2; xargs hunter2; sudo hunter2",
            "'git' hunter2 && git hunter2 && cargo hunter2",
            "bash -c 'printf %s hunter2'",
            "a | hunter2 | b && (hunter2) && $(hunter2) && `hunter2`",
            "cargo test -- hunter2",
            "cargo +hunter2 test",
            "case $l in a) ( [[ -n x ]]);; TOKEN_hunter2=*) :;; esac",
            "\"hunter2=x\" cargo test",
            "'hunter2=1'",
            "for user@hunter2:pw in a; do :; done",
            "for 'hunter2' in a; do :; done",
            "for x=hunter2 in a; do :; done",
            "select x=$(cat ~/.hunter2) in a; do :; done",
            // A substitution inside a quote inside a substitution, and a
            // comment's quote inside one.
            "x=$(echo \"$(echo \"a); cat --hunter2; b\")\")",
            "x=$(true # don't\n); echo 'a); cat --hunter2 b'",
            "x=$(true # don't\n); echo 'a); cat --hunter2 b' # it's",
            "a=(x # don't\n); echo 'a); cat --hunter2 b' # it's",
            // A program that runs another: the other's flags are its.
            "timeout 60 mytool --hunter2",
            "env -i FOO=1 mytool --hunter2",
            "xargs -n1 mytool --hunter2",
            "find . -name x -exec mytool --hunter2 {} \\;",
            "nohup mytool --hunter2",
            "sh x.sh --hunter2",
            "bash -x x.sh --hunter2",
            "git bisect run mytool --hunter2",
            "git submodule foreach mytool --hunter2",
            "rustup run stable mytool --hunter2",
            // A heredoc's delimiter of digits glued to a redirection.
            "cat <<1>/dev/null\ncat --hunter2\n1",
            // The process's id before a quote.
            "echo $$'a\\' x ' ; cat --hunter2 y'",
            "echo $($$'a\\' x ' ; cat --hunter2 y')",
            // An ANSI-C escape that is a quote in a shell's script.
            "bash -c $'echo \\x27a); cat --hunter2 b\\x27'",
            "bash -c $'echo \\047a); cat --hunter2 b\\u0027'",
            // A `case` in a function's body, a pattern `esac`, a `case`
            // in a substitution.
            "function f { case $1 in a) :;; hunter2=x|b) :;; esac; }",
            "case x in (esac) :;; hunter2=x|b) :;; esac",
            "x=$(case $1 in a) :;; hunter2=x|b) :;; esac)",
            // Not read whole: what is open at the end, a `)` that closes
            // nothing.
            "git status; echo 'a; cat --hunter2",
            "git status; echo \"a; cat --hunter2",
            "( cat --hunter2",
            "echo $(cat --hunter2",
            "echo `cat --hunter2",
            "[[ -n x && cat --hunter2",
            "case $x in a) cat --hunter2",
            "a) cat --hunter2",
            "echo $'a; cat --hunter2",
            // A here-string in a group, `\c` before its close, a comment
            // glued after `((…))` or a subshell's `)`, a quoted heredoc
            // delimiter in a group, a conditional expression over lines,
            // an arithmetic `$[…]`.
            "cat $(cat <<< x\necho '\nx\n) --hunter2 ('\n) #')",
            "cat $'\\c' ' --hunter2 ' #'",
            "((x))#'\necho 'x; cat --hunter2 y' #'",
            "cat $( (true)#) --hunter2 (\n)",
            "cat $(cat <<'E F'\n'\nE\n) --hunter2 (\nE F\n)",
            "cat $(cat <<'E)'\n) --hunter2 (\nE)\n)",
            "[[ $a == b ||\ngit == --hunter2 ]]",
            "cat $[ --hunter2 ]",
            // `cargo run`'s words are the binary's.
            "cargo run x --hunter2",
            "cargo +nightly run --release x --hunter2",
            "git -C dir submodule --quiet foreach x --hunter2",
            // An option's value that looks like a flag, after `=` is not
            // there; a global option's value before `run`.
            "grep -e --hunter2 file",
            "git log --grep --hunter2",
            "rg -g --hunter2 x",
            "tail -n5 --hunter2",
            "cargo test --features --hunter2",
            "cargo --color never run arg --hunter2",
            "cargo --config x run arg --hunter2",
            "jq --arg k --hunter2 .",
            // An option's name quoted is still the option.
            "grep '-e' --hunter2 file",
            "grep -e\"\" --hunter2 file",
            "git log \"--grep\" --hunter2",
            "cargo '--color' never run x --hunter2",
            // `fd -x` runs a command; cargo's `r` is its `run`; git's
            // options that take a value for some subcommand.
            "fd -x mytool --hunter2",
            "cargo r x --hunter2",
            "git commit --squash --hunter2",
            "git log --skip --hunter2",
            "git archive --remote --hunter2",
            "git diff -l --hunter2",
            // An empty quoted heredoc delimiter in a group.
            "cat $(cat <<''\n) --hunter2 (\n\n)",
            "cat $(cat <<\"\"\n) --hunter2 (\n\n)",
            // An operand that looks like a flag, past where a parser stops
            // at its first operand or after an option of more values; an
            // unknown subcommand's words.
            "git -c test.foo=--hunter2 config --get-regexp 'test.*' --hunter2",
            "git config --get-regexp 'test.*' --hunter2",
            "git config --replace-all x.y v --hunter2",
            "git update-index --cacheinfo 100644 abc --hunter2",
            "ls dir --hunter2",
            "grep -i pattern file --hunter2",
            "cargo foo --hunter2",
            "cargo --locked foo --hunter2",
            "dagq plan x --hunter2",
            // A `+…` word is an operand but for a toolchain's first word.
            "POSIXLY_CORRECT=1 grep +pattern --hunter2",
            "grep +pattern --hunter2",
            "git +x --hunter2",
            // A line of an unquoted heredoc's body that ends with a `\`
            // goes on at the next, at the top and in a group.
            "cat <<EOF\nx\\\nEOF\ngit --hunter2\nEOF\n",
            "cat <<-EOF\n\tx\\\n\tEOF\ngit --hunter2\nEOF\n",
            "cat <<EOF\nx\\\\\\\nEOF\ngit --hunter2\nEOF\n",
            "x=$(cat <<EOF\nx\\\nEOF\ngit --hunter2 )\nEOF\n)",
        ] {
            let shape = shape(command, &known).unwrap();
            let text = serde_json::to_string(&shape).unwrap();
            assert!(!text.contains("hunter2"), "{command}: {text}");
            for part in &shape.parts {
                for lead in &part.lead {
                    assert!(RESERVED.contains(&lead.as_str()), "{command}: {text}");
                }
                for name in &part.assigns {
                    assert!(
                        hex(name) || assigned(&format!("{name}=")).is_some(),
                        "{command}: {text}"
                    );
                }
                let subs = part
                    .program
                    .as_deref()
                    .and_then(|name| known.of_program(name));
                assert!(
                    part.program.as_deref().is_none_or(hex) || subs.is_some(),
                    "{command}: {text}"
                );
                for arg in &part.args {
                    match arg {
                        ShapeArg::Sub { sub } => {
                            assert!(
                                subs.is_some_and(|list| list.subcommands.contains(&sub.as_str()))
                            );
                        }
                        ShapeArg::Flag { flag, value } => {
                            assert!(subs.is_some() && flag_name(flag), "{command}: {text}");
                            assert!(value.as_deref().is_none_or(hex), "{command}: {text}");
                        }
                        ShapeArg::Word { word: digest } | ShapeArg::Plus { plus: digest } => {
                            assert!(hex(digest), "{command}: {text}");
                        }
                        ShapeArg::AfterDashdash { .. } => {}
                    }
                }
            }
        }
        // A conditional expression is one part of its own, its `]]` closing
        // it though an operator is glued to it.
        for command in [
            "[[ \"$value\" == ok || hunter2 == \"$value\" ]] && git status",
            "[[ -f x ]]&& git status",
            "[[ -n $a ]]||git status",
            "( [[ -n x ]]) && git status",
        ] {
            let parts = &value(command)["parts"];
            let last = &parts[parts.as_array().unwrap().len() - 1];
            assert_eq!(last["program"], "git", "{command}: {parts}");
            assert_eq!(
                last["args"],
                json!([{"sub": "status"}]),
                "{command}: {parts}"
            );
        }
        assert_eq!(
            value("[[ \"$value\" == ok || hunter2 == \"$value\" ]] && git status")["parts"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        // A command not read whole keeps no word in plain, a listed
        // program's neither.
        assert_eq!(
            value("git status; echo 'x"),
            json!({"parts": [
                {"args": [{"word": d("git")}, {"word": d("status")}]},
                {"args": [{"word": d("echo")}, {"word": d("x")}]}]})
        );
        // A program that runs another keeps its own flags before it.
        assert_eq!(
            value("timeout -s KILL 60 cargo test --hunter2")["parts"][0],
            json!({"program": "timeout", "args": [{"flag": "-s"}, {"word": d("KILL")},
                {"word": d("60")}, {"word": d("cargo")}, {"word": d("test")}, {"word": d("--hunter2")}]})
        );
        assert_eq!(
            value("git bisect run x --hunter2")["parts"][0]["args"],
            json!([{"sub": "bisect"}, {"word": d("run")}, {"word": d("x")}, {"word": d("--hunter2")}])
        );
        assert_eq!(
            value("grep -e --confidential-pattern file")["parts"][0],
            json!({"program": "grep", "args": [{"flag": "-e"}, {"word": d("--confidential-pattern")},
                {"word": d("file")}]})
        );
        assert_eq!(
            value("cargo --color never run arg --hunter2")["parts"][0]["args"],
            json!([{"flag": "--color"}, {"word": d("never")}, {"word": d("run")}, {"word": d("arg")},
                {"word": d("--hunter2")}])
        );
        // A `+…` word other than a toolchain's is an operand: GNU grep under
        // POSIXLY_CORRECT reads what follows as files, and clap's cargo reads
        // a flag after it.
        assert_eq!(
            value("POSIXLY_CORRECT=1 grep +pattern --confidential-file")["parts"][0],
            json!({"assigns": ["POSIXLY_CORRECT"], "program": "grep", "args": [
                {"word": d("+pattern")}, {"word": d("--confidential-file")}]})
        );
        assert_eq!(
            value("cargo test +x --locked")["parts"][0]["args"],
            json!([{"sub": "test"}, {"word": d("+x")}, {"flag": "--locked"}])
        );
        // After an option that takes no value, an attached value or a word
        // that is no flag, a flag keeps its name.
        assert_eq!(
            value("ls -la --color && cargo test --locked --release && git log --oneline --graph")["parts"],
            json!([
                {"program": "ls", "args": [{"flag": "-l", "value": d("a")}, {"flag": "--color"}]},
                {"program": "cargo", "args": [{"sub": "test"}, {"flag": "--locked"}, {"flag": "--release"}]},
                {"program": "git", "args": [{"sub": "log"}, {"flag": "--oneline"}, {"flag": "--graph"}]},
            ])
        );
        assert_eq!(
            value("cargo run --release x --hunter2")["parts"][0]["args"],
            json!([{"sub": "run"}, {"flag": "--release"}, {"word": d("x")}, {"word": d("--hunter2")}])
        );
        // `nextest run` is no `cargo run`: its flags stay its own.
        assert_eq!(
            value("cargo nextest run --test it --no-fail-fast")["parts"][0]["args"],
            json!([{"sub": "nextest"}, {"sub": "run"}, {"flag": "--test"}, {"word": d("it")},
                {"flag": "--no-fail-fast"}])
        );
    }

    /// What the shell reads whole is read as it reads it: an ANSI-C
    /// escape in a shell's script, a comment and a nested quote in a
    /// substitution, a function's `case`, a `case` that ends the command.
    #[test]
    fn what_the_shell_reads_whole_keeps_its_shape() {
        for (command, same) in [
            (
                "bash -c $'git status\\x3b cargo test --locked'",
                "git status; cargo test --locked",
            ),
            ("bash -c $'git status\\n'", "git status"),
            (
                "x=$(git log # don't\n) && git status",
                "x=$(y) && git status",
            ),
            ("x=\"$(echo \"$(date)\")\" git status", "x=1 git status"),
            ("echo $$ && git status", "echo 1 && git status"),
            (
                "function f { case $1 in a) git status;; esac; }",
                "f() { case $1 in a) git status;; esac; }",
            ),
        ] {
            let parts = |command: &str| value(command)["parts"].as_array().unwrap().clone();
            let (got, want) = (parts(command), parts(same));
            assert_eq!(
                got.last(),
                want.last(),
                "{command}: {got:?} against {same}: {want:?}"
            );
        }
        for command in [
            "x=$(jq . <<< \"$a\"\n) && git status",
            "x=$( (( y <<= 1 ))\n) && git status",
            "x=$(cat <<'E F'\nE\nE F\n) && git status",
            "((x))# don't\ngit status",
            "x=$[1 + 2] git status",
            "x=$(( y << 2 )) && git status",
            "x=$((1<<(2))) && git status",
            "x=$(cat <<''\nsecret\n\n) && git status",
            // A quoted delimiter's lines do not go on past a `\`; an
            // unquoted one's joined line may be the delimiter.
            "cat <<'EOF'\nx\\\nEOF\ngit status",
            "cat <<\\EOF\nx\\\nEOF\ngit status",
            "cat <<EOF\nx\\\\\nEOF\ngit status",
            "cat <<EOF\nE\\\nOF\ngit status",
            "cat <<-EOF\n\tE\\\nOF\ngit status",
            "x=$(cat <<'EOF'\nx\\\nEOF\n) && git status",
            "x=$(cat <<EOF\nE\\\nOF\n) && git status",
        ] {
            let parts = &value(command)["parts"];
            let last = &parts[parts.as_array().unwrap().len() - 1];
            assert_eq!(last["program"], "git", "{command}: {parts}");
        }
        assert_eq!(
            value("[[ -n $a &&\n-n $b ]] && git status")["parts"][1]["program"],
            "git"
        );
        assert_eq!(
            value("case $x in a) git status;; esac")["parts"][2]["program"],
            "git"
        );
        assert_eq!(
            value("cat <<1>/dev/null\nhunter2\n1\ngit status")["parts"],
            json!([{"program": "cat", "args": []}, {"program": "git", "args": [{"sub": "status"}]}])
        );
    }

    /// A positional secret is no subcommand: after a word a secret may
    /// follow, in any case; an escaped parenthesis does not close a
    /// substitution, whose text stays in its digest.
    #[test]
    fn positional_secrets_and_escaped_closes_keep_no_text() {
        for (command, secret) in [
            ("redis-cli AUTH hunter2", "hunter2"),
            ("tool login hunter2", "hunter2"),
            ("tool auth token hunter2", "token"),
            (
                "echo $(printf '%s\\n' \\) sk-live-abc123) done",
                "sk-live-abc123",
            ),
        ] {
            let text = value(command).to_string();
            assert!(!text.contains(secret), "{secret} of {command} in {text}");
        }
        assert_eq!(
            value("docker login hunter2")["parts"][0]["args"],
            json!([{"word": d("login")}, {"word": d("hunter2")}])
        );
        // Subcommands still go two deep.
        assert_eq!(
            value("cargo nextest run")["parts"][0]["args"],
            json!([{"sub": "nextest"}, {"sub": "run"}])
        );
        let shape = value("echo $(printf '%s\\n' \\) sk-live-abc123) done");
        assert_eq!(shape["parts"].as_array().unwrap().len(), 1, "{shape}");
        // An ANSI-C quoted text is closed by its unescaped `'` alone,
        // inside a substitution too.
        for (command, secret) in [
            (r#"tool $'x\' hunter2 '"#, "hunter2"),
            (r#"echo $(printf $'x\') hunter2 ' )"#, "hunter2"),
            (r#"echo $'don\'t leak mysecret'"#, "leak"),
        ] {
            let text = value(command).to_string();
            assert!(!text.contains(secret), "{secret} of {command} in {text}");
        }
    }

    /// A heredoc's body, a comment, a command substitution and a redirection
    /// keep no text; a quoted subcommand is still one.
    #[test]
    fn heredocs_comments_substitutions_and_redirections_keep_no_text() {
        let command = "cat > notes.md <<'EOF'\nfix the bug in the parser\nEOF\ngit commit -m \"$(cat <<'X'\nsecret body line\nX\n)\" # why it is done\n'git' log";
        let shape = value(command);
        let text = shape.to_string();
        for kept_out in [
            "fix", "parser", "secret", "body", "notes", "why", "EOF", "X\"",
        ] {
            assert!(!text.contains(kept_out), "{kept_out} in {text}");
        }
        assert_eq!(
            shape,
            json!({"parts": [
                {"program": "cat", "args": []},
                {"program": "git", "args": [{"sub": "commit"}, {"flag": "-m"},
                    {"word": d("$(cat <<'X'\nsecret body line\nX\n)")}]},
                {"program": "git", "args": [{"sub": "log"}]}]})
        );
    }

    /// A shell's script (Codex's `/bin/zsh -lc '…'`) is shaped in its place.
    #[test]
    fn a_shells_script_is_shaped_as_its_parts() {
        assert_eq!(
            value("/bin/zsh -lc 'cargo test --test it && git status'"),
            value("cargo test --test it && git status")
        );
        assert_eq!(
            value("bash -c \"cargo fmt --all --check\""),
            value("cargo fmt --all --check")
        );
        // A shell running a file is a program with its words.
        assert_eq!(
            value("bash scripts/x.sh"),
            json!({"parts": [{"program": "bash", "args": [{"word": d("scripts/x.sh")}]}]})
        );
    }

    /// The subcommands are the first two words before any flag that look
    /// like one; a `+` word takes no position.
    #[test]
    fn only_the_first_two_words_before_a_flag_may_be_subcommands() {
        assert_eq!(
            value("cargo nextest run --test it"),
            json!({"parts": [{"program": "cargo", "args": [
                {"sub": "nextest"}, {"sub": "run"}, {"flag": "--test"}, {"word": d("it")}]}]})
        );
        assert_eq!(
            value("cargo +stable test claim more"),
            json!({"parts": [{"program": "cargo", "args": [
                {"plus": d("+stable")}, {"sub": "test"}, {"word": d("claim")}, {"word": d("more")}]}]})
        );
        assert_eq!(
            value("cargo Test claim"),
            json!({"parts": [{"program": "cargo", "args": [
                {"word": d("Test")}, {"word": d("claim")}]}]})
        );
        assert_eq!(
            value("cargo -q test"),
            json!({"parts": [{"program": "cargo", "args": [{"flag": "-q"}, {"word": d("test")}]}]})
        );
        assert_eq!(
            value("git a-very-long-subcommand-name-x"),
            json!({"parts": [{"program": "git", "args": [{"word": d("a-very-long-subcommand-name-x")}]}]})
        );
    }

    /// A quoted text or a glued quoted value that starts with `-`, a
    /// heredoc inside a substitution whose body closes a parenthesis or
    /// opens a quote, and a first word that is a substitution keep no
    /// text.
    #[test]
    fn texts_that_look_like_flags_or_close_a_substitution_keep_nothing() {
        let commit = "git commit -m \"- fix the parser bug in secret module\"";
        assert_eq!(
            value(commit),
            json!({"parts": [{"program": "git", "args": [{"sub": "commit"}, {"flag": "-m"},
                {"word": d("- fix the parser bug in secret module")}]}]})
        );
        for command in [
            "curl -H\"Authorization: Bearer xyz\" https://example.com",
            "git commit -m \"$(cat <<'EOF'\nfix a) case\nsay \"hi\nsecret plan don't\nEOF\n)\"",
            "echo $(cat <<EOF\nfix a) secret plan\nEOF\n) done",
            "$(printf 'my token here') run",
            "FOO=1 bash -c 'for \"$(secret plan)\" in x; do y; done'",
        ] {
            let text = value(command).to_string();
            for kept_out in ["Bearer", "xyz", "secret", "plan", "token", "parser"] {
                assert!(
                    !text.contains(kept_out),
                    "{kept_out} in {text} of {command}"
                );
            }
        }
        assert_eq!(
            value("echo $(cat <<EOF\nfix a) secret plan\nEOF\n) done")["parts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            value("FOO=1 bash -c 'cargo test'")["parts"][0]["assigns"],
            json!(["FOO"])
        );
    }

    /// A shape too large for its bound is cut from its end and marked.
    #[test]
    fn a_shape_past_its_bound_is_cut_and_marked() {
        let command = format!("cat {}", vec!["x"; 400].join(" "));
        let shape = value(&command);
        assert_eq!(shape["truncated"], true);
        assert!(
            serde_json::to_vec(&shape).unwrap().len() <= crate::domain::telemetry::MAX_SHAPE_BYTES
        );
        assert_eq!(shape["parts"][0]["program"], "cat");
        assert_eq!(value("")["parts"], json!([]));
    }
}
