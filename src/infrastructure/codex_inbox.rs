//! The Codex inbox (ADR-t1181-1 decision 2, ADR-t3331-1): the interactive
//! Codex TUI `dagq inbox` starts, given everything it needs by its launch
//! arguments and nothing written to the person's Codex settings
//! (`$CODEX_HOME`, else `~/.codex`) or `~/.claude`: the hooks
//! ([`hook_configs`]) that `dagq codex-inbox-hook` runs
//! ([`run_hook`]), each trusted for that start alone, the inbox's
//! instructions ([`developer_instructions`]), the main checkout trusted for
//! the start, the settings that stop what the TUI would write itself
//! ([`LAUNCH_CONFIG`]), and the read-only sandbox that
//! [`crate::application::AgentProvider::reach_queue_service`] then opens
//! to the queue service's socket alone. The one setting the TUI writes
//! that no argument stops is checked before the start instead
//! ([`check_user_config`]).
//!
//! What is here was read from openai/codex's source at `rust-v0.160.0`
//! and measured with codex-cli 0.160.0 (`--no-daemon`, matcherless command
//! hooks); the source of `rust-v0.164.0` computes the keys and hashes of
//! hooks given with `-c` the same way. A Codex that computes them
//! otherwise shows the hooks whose hash no longer matches on its trust
//! screen and runs none of them untrusted.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::codex::{JOB_SANDBOX, trust_config};
use crate::application::{
    CommandSpec,
    codex_inbox::{InboxHook, guard, session_start_context, shell_word, stop_decision},
};

/// The hidden subcommand a Codex inbox's hooks run: `dagq
/// codex-inbox-hook <hook>` ([`InboxHook::argument`]).
pub const HOOK_COMMAND: &str = "codex-inbox-hook";

/// The source Codex names a hook given with `-c` by in its key: the
/// session flags' layer's synthetic path (`hooks/src/discovery.rs`
/// `config_toml_source_path`, `/` in front but on Windows). Codex reads
/// `hooks.state` only from the user's layer and this one
/// (`hooks/src/config_rules.rs` `hook_states_from_stack`), so a
/// `hooks.state` given with `-c` trusts for that start alone and a
/// project's `.codex/` cannot trust its own hooks.
pub const SESSION_FLAGS_SOURCE: &str = "/<session-flags>/config.toml";

/// The `-c` every Codex inbox starts with besides its hooks, sandbox and
/// trust: no update check at the start, and no tooltips. `tui.show_tooltips
/// = false` makes the TUI return before it picks a model's availability
/// notice (`tui/src/app/startup_prompts.rs`
/// `prepare_startup_tooltip_override`), which would otherwise rewrite
/// `[tui.model_availability_nux]` in the person's `config.toml` with the
/// count of its notices (read in the source; not seen in a logged-in
/// start). Neither `-c tui.model_availability_nux={…}` (a model it does
/// not name gets its notice, and the merged table, the `-c` values in it,
/// is written to the person's file) nor `-c
/// tui.screen_reader_detection_done=true` (the TUI looks at the user's
/// layer only, [`check_user_config`]) is given. A setting a later Codex is
/// found to write by itself is added here when an argument stops it, else
/// checked like that one (and the inbox not opened when neither can be).
pub const LAUNCH_CONFIG: [&str; 2] = [
    "check_for_update_on_startup=false",
    "tui.show_tooltips=false",
];

/// The event of `hook` as Codex names it in `-c hooks.<event>`.
fn event(hook: InboxHook) -> &'static str {
    match hook {
        InboxHook::SessionStart => "SessionStart",
        InboxHook::SessionEnd => "SessionEnd",
        InboxHook::Stop => "Stop",
        InboxHook::PreToolUse => "PreToolUse",
    }
}

/// The label of `hook`'s event in its key and hash
/// (`hooks/src/lib.rs` `hook_event_key_label`).
fn label(hook: InboxHook) -> &'static str {
    match hook {
        InboxHook::SessionStart => "session_start",
        InboxHook::SessionEnd => "session_end",
        InboxHook::Stop => "stop",
        InboxHook::PreToolUse => "pre_tool_use",
    }
}

/// The timeout, in seconds, Codex gives a command hook that names none,
/// which its hash holds (`normalize_command_hook`): 1 for SessionEnd
/// (capped at 3 when named), 600 for the others. A SessionEnd hook is
/// stopped after 1 second.
fn default_timeout(hook: InboxHook) -> u64 {
    match hook {
        InboxHook::SessionEnd => 1,
        _ => 600,
    }
}

/// The key of the `handler`th handler of the `group`th group of `hook`
/// given with `-c` (`hooks/src/lib.rs` `hook_key`):
/// `<source>:<event label>:<group>:<handler>`, the source
/// [`SESSION_FLAGS_SOURCE`].
pub fn hook_key(hook: InboxHook, group: usize, handler: usize) -> String {
    format!("{SESSION_FLAGS_SOURCE}:{}:{group}:{handler}", label(hook))
}

/// The trusted hash of a command hook of the event labelled `label` that
/// runs `command` with `timeout` and names no matcher, status message or
/// context limit (`hooks/src/discovery.rs` `hook_hash`): `sha256:<hex>` of
/// the normalized hook `{event_name, hooks: [{type: "command", command,
/// timeout, async: false}]}` as `config/src/fingerprint.rs`
/// `version_for_toml` writes it, JSON with its object keys sorted and no
/// whitespace. A matcher would enter the hash normalized by
/// `matcher_pattern_for_event`, which was not read: no hook here names
/// one.
pub fn hook_hash(label: &str, command: &str, timeout: u64) -> String {
    // serde_json's maps are sorted by key, as `version_for_toml` sorts.
    let normalized = json!({
        "event_name": label,
        "hooks": [{"type": "command", "command": command, "timeout": timeout, "async": false}],
    });
    let digest = Sha256::digest(normalized.to_string().as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// The command of `hook`: `<dagq> codex-inbox-hook <hook>`, `dagq` the
/// binary `dagq inbox` runs as. Its text is in the hook's hash, so the
/// hash is made from the same text.
pub fn hook_command(dagq: &Path, hook: InboxHook) -> Result<String> {
    let dagq = dagq
        .to_str()
        .with_context(|| format!("{} is not UTF-8", dagq.display()))?;
    Ok(format!(
        "{} {HOOK_COMMAND} {}",
        shell_word(dagq),
        hook.argument()
    ))
}

/// The `-c` of the inbox's hooks: each [`InboxHook`] as one group of one
/// matcherless command handler, `hooks.<Event>=[{hooks=[{type="command",
/// command="…"}]}]`, then `hooks.state={"<key>"={trusted_hash="…"},…}` with
/// every one of their keys ([`hook_key`]) and hashes ([`hook_hash`]): a
/// Codex trusts each hook on its own, so one left out (or whose hash Codex
/// computes otherwise) would be the one not run. One inline table, as a
/// key holds `.`, which a dotted `-c` key would split at. No hook is given
/// a matcher: the guardrail's lets every tool but the shell pass itself
/// ([`guard`]). Nothing trusts the hooks dagq does not give: a
/// project's or the person's still shows on Codex's trust screen.
pub fn hook_configs(dagq: &Path) -> Result<Vec<String>> {
    let mut configs = Vec::new();
    let mut state = Vec::new();
    for hook in InboxHook::ALL {
        let command = hook_command(dagq, hook)?;
        configs.push(format!(
            "hooks.{}=[{{hooks=[{{type=\"command\",command={}}}]}}]",
            event(hook),
            serde_json::to_string(&command)?
        ));
        state.push(format!(
            "{}={{trusted_hash=\"{}\"}}",
            serde_json::to_string(&hook_key(hook, 0, 0))?,
            hook_hash(label(hook), &command, default_timeout(hook))
        ));
    }
    configs.push(format!("hooks.state={{{}}}", state.join(",")));
    Ok(configs)
}

/// The `SKILL.md` of the dagq-inbox skill in the plugin directory
/// `plugin_dir`.
fn skill_file(plugin_dir: &Path) -> PathBuf {
    plugin_dir
        .join("skills")
        .join("dagq-inbox")
        .join("SKILL.md")
}

/// The Codex inbox's instructions that hold for the whole session
/// (`developer_instructions`), in place of the plugin's skill a Codex
/// session cannot load: read the dagq-inbox skill (in `plugin_dir`, else
/// the installed plugin's) and follow it, with Codex's differences: the
/// inbox's watch in the foreground of the turn, an ask's options as a
/// numbered list, and the queue through `dagq` only.
pub fn developer_instructions(dagq: &Path, plugin_dir: Option<&Path>) -> String {
    let skill = match plugin_dir {
        Some(dir) => format!("the dagq-inbox skill at {}", skill_file(dir).display()),
        None => "the dagq-inbox skill of the installed claude-dagq plugin (its skills/dagq-inbox/SKILL.md under ~/.claude/plugins)".to_owned(),
    };
    let watch =
        crate::application::codex_inbox::foreground_watch(&dagq.display().to_string(), None);
    format!(
        "This Codex session is the inbox of a dagq queue (DAGQ_ROLE=inbox). Read {skill} and the references beside it before anything else, and follow it with these differences for Codex. \
         1. There are no background tasks: run the inbox's watch in the foreground, {watch} --after <cursor>, which returns within its timeout; handle what it returns, then run it again from the cursor it returns, and keep doing so whenever the person is not talking to you. \
         2. There is no question tool: show an ask's question and its options as a numbered list, and take the person's reply as text. \
         3. Reach the queue only with the dagq CLI through the shell tool, never its database. \
         At each start, compaction and /clear a hook prints `dagq status --role inbox` with the cursor to watch from."
    )
}

/// The Codex inbox's command: `codex --no-daemon` (the person's shared
/// Codex daemon is not used) in `cwd`, the main checkout, trusted for this
/// start ([`trust_config`], so Codex persists no trust), with
/// [`LAUNCH_CONFIG`], [`developer_instructions`], [`hook_configs`] and
/// `--sandbox read-only` (no write anywhere; the caller's
/// `reach_queue_service` replaces it with the profile that also reaches
/// the queue service's socket), then the prompt after `--`. No argument
/// skips the trust of every hook (`--dangerously-bypass-hook-trust`) or
/// the sandbox and approvals, and nothing is written. A `codex_home`
/// ([`launch_codex_home`]) is given as `CODEX_HOME`, the absolute path the
/// check before the start read.
pub fn inbox_command(
    executable: &Path,
    dagq: &Path,
    cwd: &Path,
    codex_home: Option<&Path>,
    plugin_dir: Option<&Path>,
    prompt: &str,
) -> Result<CommandSpec> {
    let mut command = CommandSpec::new(executable);
    command.current_dir(cwd).arg("--no-daemon");
    if let Some(home) = codex_home {
        command.env("CODEX_HOME", home);
    }
    let configs = [trust_config(cwd)?]
        .into_iter()
        .chain(LAUNCH_CONFIG.map(str::to_owned))
        .chain([format!(
            "developer_instructions={}",
            serde_json::to_string(&developer_instructions(dagq, plugin_dir))?
        )])
        .chain(hook_configs(dagq)?);
    for config in configs {
        command.arg("-c").arg(config);
    }
    command
        .arg("--sandbox")
        .arg(JOB_SANDBOX)
        .arg("--")
        .arg(prompt);
    Ok(command)
}

/// The line a person adds to their Codex `config.toml` for a Codex inbox
/// to open: its `[tui]` table's `screen_reader_detection_done`.
pub const SCREEN_READER_LINES: &str = "[tui]\nscreen_reader_detection_done = true";

/// `codex_home` (`$CODEX_HOME`) as the Codex inbox reads it: `None` when
/// unset or empty, else the path, a relative one taken from `cwd`, the
/// directory Codex starts in (the main checkout), as Codex resolves it
/// there. The inbox's Codex is given this absolute path
/// ([`inbox_command`]), so the file checked before the start
/// ([`user_config_file`]) is the one it reads, wherever `dagq inbox` was
/// typed.
pub fn launch_codex_home(codex_home: Option<OsString>, cwd: &Path) -> Option<PathBuf> {
    codex_home
        .filter(|dir| !dir.is_empty())
        .map(|dir| cwd.join(dir))
}

/// The person's Codex `config.toml` the Codex inbox started in `cwd`
/// reads: under [`launch_codex_home`] when `$CODEX_HOME` (`codex_home`) is
/// set and not empty, else `~/.codex` of `home` (`$HOME`, a relative one
/// taken from `cwd` too).
pub fn user_config_file(
    codex_home: Option<OsString>,
    home: Option<OsString>,
    cwd: &Path,
) -> Option<PathBuf> {
    let codex_home = launch_codex_home(codex_home, cwd).or_else(|| {
        home.filter(|dir| !dir.is_empty())
            .map(|home| cwd.join(home).join(".codex"))
    })?;
    Some(codex_home.join("config.toml"))
}

/// Whether the TOML `text` has `tui.screen_reader_detection_done`,
/// whatever its value: as a key of a `[tui]` table, as a dotted key
/// (`tui.screen_reader_detection_done` at the top, or under a table), or
/// in an inline `tui = {…}`. The file is read by its structure
/// ([`TomlScan`]): strings (multi-line ones included), comments, arrays
/// and inline tables are skipped as values, so a `[tui]` written inside a
/// string is no table. A file it cannot read through counts as missing
/// the setting, so the inbox stays closed rather than let Codex write.
pub fn has_screen_reader_field(text: &str) -> bool {
    TomlScan::new(text)
        .has(&["tui", "screen_reader_detection_done"])
        .unwrap_or(false)
}

/// A reader of a TOML document's structure: its table headers, its keys
/// and the first-level keys of their inline tables, with every value
/// skipped as TOML reads it. `Err` is a document it cannot read through.
struct TomlScan {
    text: Vec<char>,
    at: usize,
}

impl TomlScan {
    fn new(text: &str) -> Self {
        Self {
            text: text.trim_start_matches('\u{feff}').chars().collect(),
            at: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.text.get(self.at).copied()
    }

    fn starts(&self, with: &str) -> bool {
        with.chars()
            .enumerate()
            .all(|(n, c)| self.text.get(self.at + n) == Some(&c))
    }

    /// Spaces and tabs.
    fn blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.at += 1;
        }
    }

    /// Blanks, newlines and comments.
    fn space(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => self.at += 1,
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.at += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// Whether the document has the key `path` (its table's and its own
    /// parts), as a key or in an inline table one level up.
    fn has(&mut self, path: &[&str]) -> Result<bool, ()> {
        let mut table: Vec<String> = Vec::new();
        loop {
            self.space();
            match self.peek() {
                None => return Ok(false),
                Some('[') => {
                    self.at += 1;
                    let array = self.peek() == Some('[');
                    if array {
                        self.at += 1;
                    }
                    table = self.key()?;
                    for _ in 0..=usize::from(array) {
                        self.blanks();
                        if self.peek() != Some(']') {
                            return Err(());
                        }
                        self.at += 1;
                    }
                }
                Some(_) => {
                    let mut key = table.clone();
                    key.extend(self.key()?);
                    if self.peek() != Some('=') {
                        return Err(());
                    }
                    self.at += 1;
                    let inline = self.value()?;
                    if key == path {
                        return Ok(true);
                    }
                    if key[..] == path[..path.len() - 1]
                        && inline.iter().any(|inner| inner == &path[path.len() - 1..])
                    {
                        return Ok(true);
                    }
                }
            }
            // The rest of the line holds nothing but a comment.
            self.blanks();
            match self.peek() {
                None | Some('\n' | '\r' | '#') => {}
                _ => return Err(()),
            }
        }
    }

    /// A dotted key's parts, bare or quoted, with the blanks after it.
    fn key(&mut self) -> Result<Vec<String>, ()> {
        let mut parts = Vec::new();
        loop {
            self.blanks();
            let part = match self.peek() {
                Some('"') => self.basic()?,
                Some('\'') => self.literal()?,
                _ => {
                    let start = self.at;
                    while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    {
                        self.at += 1;
                    }
                    if start == self.at {
                        return Err(());
                    }
                    self.text[start..self.at].iter().collect()
                }
            };
            parts.push(part);
            self.blanks();
            if self.peek() != Some('.') {
                return Ok(parts);
            }
            self.at += 1;
        }
    }

    /// A basic string (`"…"`, or `"""…"""` over lines), its escapes decoded
    /// ([`Self::escape`]).
    fn basic(&mut self) -> Result<String, ()> {
        let multi = self.starts("\"\"\"");
        self.at += if multi { 3 } else { 1 };
        let mut text = String::new();
        loop {
            if multi && self.starts("\"\"\"") {
                self.at += 3;
                // Up to two quotes more belong to the string.
                while self.peek() == Some('"') {
                    self.at += 1;
                }
                return Ok(text);
            }
            match self.peek() {
                None => return Err(()),
                Some('\n') if !multi => return Err(()),
                Some('"') if !multi => {
                    self.at += 1;
                    return Ok(text);
                }
                Some('\\') => {
                    self.at += 1;
                    self.escape(multi, &mut text)?;
                }
                Some(c) => {
                    text.push(c);
                    self.at += 1;
                }
            }
        }
    }

    /// The escape after a `\\` of a basic string, decoded into `text` as
    /// TOML 1.0 reads it (`\\b`, `\\t`, `\\n`, `\\f`, `\\r`, `\\"`, `\\\\`,
    /// `\\uXXXX`, `\\UXXXXXXXX`; in a multi-line string, a `\\` at the end
    /// of a line drops the blanks and newlines after it). A quoted key is
    /// compared decoded, as Codex reads it (`"\\u0074ui"` is `tui`). Any
    /// other escape is no TOML.
    fn escape(&mut self, multi: bool, text: &mut String) -> Result<(), ()> {
        let c = self.peek().ok_or(())?;
        self.at += 1;
        let decoded = match c {
            'b' => '\u{8}',
            't' => '\t',
            'n' => '\n',
            'f' => '\u{c}',
            'r' => '\r',
            '"' => '"',
            '\\' => '\\',
            'u' | 'U' => {
                let digits = if c == 'u' { 4 } else { 8 };
                let hex: String = self
                    .text
                    .get(self.at..self.at + digits)
                    .ok_or(())?
                    .iter()
                    .collect();
                self.at += digits;
                let code = u32::from_str_radix(&hex, 16).map_err(|_| ())?;
                char::from_u32(code).ok_or(())?
            }
            ' ' | '\t' | '\r' | '\n' if multi => {
                while matches!(self.peek(), Some(' ' | '\t' | '\r' | '\n')) {
                    self.at += 1;
                }
                return Ok(());
            }
            _ => return Err(()),
        };
        text.push(decoded);
        Ok(())
    }

    /// A literal string (`'…'`, or `'''…'''` over lines).
    fn literal(&mut self) -> Result<String, ()> {
        let multi = self.starts("'''");
        self.at += if multi { 3 } else { 1 };
        let mut text = String::new();
        loop {
            if multi && self.starts("'''") {
                self.at += 3;
                while self.peek() == Some('\'') {
                    self.at += 1;
                }
                return Ok(text);
            }
            match self.peek() {
                None => return Err(()),
                Some('\n') if !multi => return Err(()),
                Some('\'') if !multi => {
                    self.at += 1;
                    return Ok(text);
                }
                Some(c) => {
                    text.push(c);
                    self.at += 1;
                }
            }
        }
    }

    /// Skip a value; for an inline table, the first-level keys it holds.
    fn value(&mut self) -> Result<Vec<Vec<String>>, ()> {
        self.blanks();
        match self.peek() {
            Some('"') => self.basic().map(|_| Vec::new()),
            Some('\'') => self.literal().map(|_| Vec::new()),
            Some('[') => {
                self.at += 1;
                loop {
                    self.space();
                    match self.peek() {
                        Some(']') => {
                            self.at += 1;
                            return Ok(Vec::new());
                        }
                        Some(',') => self.at += 1,
                        None => return Err(()),
                        Some(_) => {
                            self.value()?;
                        }
                    }
                }
            }
            Some('{') => {
                self.at += 1;
                let mut keys = Vec::new();
                loop {
                    self.space();
                    match self.peek() {
                        Some('}') => {
                            self.at += 1;
                            return Ok(keys);
                        }
                        Some(',') => self.at += 1,
                        None => return Err(()),
                        Some(_) => {
                            let key = self.key()?;
                            if self.peek() != Some('=') {
                                return Err(());
                            }
                            self.at += 1;
                            self.value()?;
                            keys.push(key);
                        }
                    }
                }
            }
            Some(_) => {
                // A number, a boolean or a date: up to what ends a value.
                let start = self.at;
                while !matches!(
                    self.peek(),
                    None | Some('\n' | '\r' | ',' | ']' | '}' | '#')
                ) {
                    self.at += 1;
                }
                while self.at > start && matches!(self.text[self.at - 1], ' ' | '\t') {
                    self.at -= 1;
                }
                if start == self.at {
                    return Err(());
                }
                Ok(Vec::new())
            }
            None => Err(()),
        }
    }
}

/// Whether a Codex inbox may start on the person's Codex settings
/// (`config_file`, [`user_config_file`]): the TUI writes `[tui]
/// screen_reader_detection_done = true` (and, with a screen reader on,
/// `animations = false`) into the user's `config.toml` at its first start
/// when that file lacks the setting, deciding by the user's layer alone
/// (`tui/src/screen_reader.rs`), which no `-c` reaches. So the inbox
/// starts only when the file has it ([`has_screen_reader_field`]); a file
/// missing or unreadable, or without it, is refused with why, where the
/// file is and the lines a person adds themselves. The file is only read.
pub fn check_user_config(config_file: Option<&Path>) -> Result<(), String> {
    let add = format!(
        "add these lines to it yourself and run `dagq inbox` again (dagq does not write it):\n{SCREEN_READER_LINES}"
    );
    let Some(file) = config_file else {
        return Err(format!(
            "the Codex inbox was not opened: neither CODEX_HOME nor HOME is set, so Codex's config.toml cannot be found to check that Codex will not write to it; {add}"
        ));
    };
    let shown = file.display();
    match std::fs::read(file) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) if has_screen_reader_field(&text) => Ok(()),
            Ok(_) => Err(format!(
                "the Codex inbox was not opened: {shown} has no tui.screen_reader_detection_done, which Codex's TUI would write into it at its start; {add}"
            )),
            Err(_) => Err(format!(
                "the Codex inbox was not opened: {shown} is not UTF-8, so whether Codex's TUI would write tui.screen_reader_detection_done into it cannot be checked; {add}"
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "the Codex inbox was not opened: {shown} does not exist, and Codex's TUI would create it with tui.screen_reader_detection_done; create it with the lines below yourself: {add}"
        )),
        Err(error) => Err(format!(
            "the Codex inbox was not opened: {shown} could not be read ({error}), so whether Codex's TUI would write tui.screen_reader_detection_done into it cannot be checked; {add}"
        )),
    }
}

/// [`check_user_config`] on this process's `CODEX_HOME` and `HOME`, for
/// the Codex inbox started in `cwd`.
pub fn check_this_user_config(cwd: &Path) -> Result<(), String> {
    let file = user_config_file(
        std::env::var_os("CODEX_HOME"),
        std::env::var_os("HOME"),
        cwd,
    );
    check_user_config(file.as_deref())
}

/// `dagq` with the queue of the inbox's environment (`DAGQ_DB`, else
/// `DAGQ_QUEUE`) as `--db`, and `args`.
fn dagq(launcher: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(launcher);
    // An explicit `DAGQ_DB` wins, as for the Claude inbox's hooks.
    let named = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(queue) =
        named("DAGQ_DB").or_else(|| named(crate::application::lifecycle::QUEUE_ENV))
    {
        command.arg("--db").arg(queue);
    }
    command.args(args);
    command
}

/// `dagq status --role inbox`: its stdout, or its stderr when it fails.
fn status(launcher: &Path) -> Result<String, String> {
    match dagq(launcher, &["status", "--role", "inbox"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).into_owned()),
        Err(error) => Err(format!("{} could not run: {error}", launcher.display())),
    }
}

/// `dagq session-event <event>` with the hook's `input` on its stdin: the
/// span of the inbox's session, recorded as the Claude inbox's hooks
/// record it. Its failure is left out: it never stops the session.
fn record_span(launcher: &Path, event: &str, input: &str) {
    let child = dagq(launcher, &["session-event", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
        let _ = child.wait();
    }
}

/// `dagq codex-inbox-hook <hook>`: reads the hook's input on stdin, does
/// what [`crate::application::codex_inbox`] decides for `hook` and prints
/// its answer on stdout. It always exits 0: a hook that fails never stops
/// the session (an input that is no JSON is let pass).
pub fn run_hook(hook: InboxHook) -> i32 {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let parsed: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let launcher = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dagq"));
    let shown = launcher.display().to_string();
    let answer = match hook {
        InboxHook::SessionStart => {
            record_span(&launcher, "open", &input);
            let status = status(&launcher);
            Some(session_start_context(
                &shown,
                status.as_deref().map_err(String::as_str),
            ))
        }
        InboxHook::SessionEnd => {
            record_span(&launcher, "close", &input);
            None
        }
        InboxHook::Stop => {
            let read = || {
                status(&launcher)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            };
            // A watch started at the end of the turn may not have written
            // its record yet: one more look a second later.
            read()
                .and_then(|status| stop_decision(&parsed, &status, &shown))
                .and_then(|_| {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    read().and_then(|status| stop_decision(&parsed, &status, &shown))
                })
                .map(|block| format!("{block}\n"))
        }
        InboxHook::PreToolUse => guard(&parsed).map(|deny| format!("{deny}\n")),
    };
    if let Some(answer) = answer {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(answer.as_bytes());
        let _ = stdout.flush();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command of each hook of the measurement whose launch arguments
    /// and `hooks.state` are recorded in
    /// `docs/plans/codex-inbox-hook-trust-spike/args/args-A-034320.txt`,
    /// with the trusted hash Codex 0.160.0 took for it: each hook's hash
    /// is checked on its own, as Codex trusts each on its own.
    const MEASURED: &str = "sh /private/var/folders/3p/g_cty8k11wqf2pg43pwc05g40000gn/T/dagq-worker-t3336-NpaEvU/bin/hook.sh dagq";

    #[test]
    fn each_hooks_key_and_hash_are_the_ones_codex_trusted() {
        let known = [
            (
                InboxHook::SessionStart,
                "/<session-flags>/config.toml:session_start:0:0",
                "sha256:8d33ed88d73d3485ae3d1d8820642940571d12980ec108eaafa67f3a9bb70ba0",
            ),
            (
                InboxHook::SessionEnd,
                "/<session-flags>/config.toml:session_end:0:0",
                "sha256:8f21f5d7ad8985c07396a7d191e5dc09febe69a7b12198a896a5dd9b00a9acef",
            ),
            (
                InboxHook::Stop,
                "/<session-flags>/config.toml:stop:0:0",
                "sha256:47f07108b959f9a6f1703a7b585d91457c6ced0940ac3a843a8547b332e6f723",
            ),
            (
                InboxHook::PreToolUse,
                "/<session-flags>/config.toml:pre_tool_use:0:0",
                "sha256:e7f5fd1a6ac4cd8da468412e25026bc1d5a59f49cba15c551918082cfcbaa5c7",
            ),
        ];
        for (hook, key, hash) in known {
            assert_eq!(hook_key(hook, 0, 0), key);
            let command = format!("{MEASURED} {}", event(hook));
            assert_eq!(
                hook_hash(label(hook), &command, default_timeout(hook)),
                hash,
                "{hook:?}"
            );
        }
    }

    /// Each hook is given with its key and the hash of its own command,
    /// all four in the one `hooks.state`.
    #[test]
    fn the_hooks_are_given_with_every_keys_hash() {
        let dagq = Path::new("/Users/me/.local/bin/dagq");
        let configs = hook_configs(dagq).unwrap();
        assert_eq!(configs.len(), 5);
        assert_eq!(
            configs[0],
            r#"hooks.SessionStart=[{hooks=[{type="command",command="/Users/me/.local/bin/dagq codex-inbox-hook session-start"}]}]"#
        );
        assert_eq!(
            configs[3],
            r#"hooks.PreToolUse=[{hooks=[{type="command",command="/Users/me/.local/bin/dagq codex-inbox-hook pre-tool-use"}]}]"#
        );
        let state = configs[4].strip_prefix("hooks.state={").unwrap();
        for hook in InboxHook::ALL {
            let command = hook_command(dagq, hook).unwrap();
            let entry = format!(
                "\"{}\"={{trusted_hash=\"{}\"}}",
                hook_key(hook, 0, 0),
                hook_hash(label(hook), &command, default_timeout(hook))
            );
            assert!(state.contains(&entry), "{state}");
        }
        // A path a shell would split is quoted, in the command and so in
        // its hash.
        let spaced = hook_command(Path::new("/opt/my dagq/dagq"), InboxHook::Stop).unwrap();
        assert_eq!(spaced, "'/opt/my dagq/dagq' codex-inbox-hook stop");
    }

    /// The Codex inbox starts with its hooks, their trust, its
    /// instructions and the read-only sandbox, stops the tooltips, and
    /// gives nothing that skips every hook's trust or the sandbox, nor the
    /// settings that do not stop what the TUI writes.
    #[test]
    fn the_codex_inbox_starts_with_its_hooks_trust_and_instructions_only() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let dagq = Path::new("/bin/dagq");
        let plugin = Path::new("/plugins/claude-dagq");
        let command = inbox_command(
            Path::new("/bin/codex"),
            dagq,
            &cwd,
            None,
            Some(plugin),
            "You are the inbox",
        )
        .unwrap();
        // The person's own CODEX_HOME is left as it is when none was set.
        assert!(command.get_envs().all(|(key, _)| key != "CODEX_HOME"));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
        assert_eq!(args[0], "--no-daemon");
        assert_eq!(&args[args.len() - 2..], ["--", "You are the inbox"]);
        let configs: Vec<&str> = args
            .windows(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].as_str())
            .collect();
        assert!(configs.contains(&trust_config(&cwd).unwrap().as_str()));
        assert!(configs.contains(&"tui.show_tooltips=false"));
        for config in hook_configs(dagq).unwrap() {
            assert!(configs.contains(&config.as_str()), "{config}");
        }
        let instructions = configs
            .iter()
            .find_map(|config| config.strip_prefix("developer_instructions="))
            .unwrap();
        let instructions: String = serde_json::from_str(instructions).unwrap();
        assert!(
            instructions.contains("/plugins/claude-dagq/skills/dagq-inbox/SKILL.md")
                && instructions.contains("/bin/dagq watch --role inbox --after <cursor>")
                && instructions.contains("in the foreground"),
            "{instructions}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--sandbox" && pair[1] == "read-only")
        );
        for config in &configs {
            assert!(
                !config.starts_with("tui.screen_reader_detection_done")
                    && !config.starts_with("tui.model_availability_nux")
                    && !config.starts_with("approval_policy")
                    && !config.starts_with("sandbox_mode"),
                "{config}"
            );
        }
        for arg in &args {
            assert!(
                !arg.contains("dangerously") && arg != "danger-full-access" && arg != "--yolo",
                "{arg}"
            );
        }
        // Nothing was written: not the repository, nor anything else here.
        assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);
        // The person's Codex settings, Codex's home and ~/.claude are not
        // named.
        for arg in &args {
            assert!(
                !arg.contains(".codex") && !arg.contains("CODEX_HOME"),
                "{arg}"
            );
        }
        assert!(
            developer_instructions(dagq, None).contains("installed claude-dagq plugin"),
            "the installed plugin's skill without a plugin directory"
        );
    }

    /// The inbox's read-only sandbox becomes the profile that also reaches
    /// the queue service's socket and nothing else: no writable root, no
    /// network but the one socket, the queue's directory not writable.
    #[test]
    fn the_codex_inbox_reaches_only_the_queue_services_socket() {
        use crate::application::AgentProvider;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("service").join("queue.sock");
        std::fs::create_dir(socket.parent().unwrap()).unwrap();
        let codex = super::super::codex::Codex::new(PathBuf::from("/bin/codex"));
        let mut command = inbox_command(
            &codex.executable,
            Path::new("/bin/dagq"),
            dir.path(),
            None,
            None,
            "p",
        )
        .unwrap();
        codex.reach_queue_service(&mut command, &socket);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg == "--sandbox"), "{args:?}");
        let real = socket
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("queue.sock");
        for config in [
            r#"default_permissions="dagq_job""#.to_owned(),
            r#"permissions.dagq_job.extends=":read-only""#.to_owned(),
            "features.network_proxy=true".to_owned(),
            format!(
                r#"permissions.dagq_job.network.unix_sockets={{{}="allow"}}"#,
                serde_json::to_string(real.to_str().unwrap()).unwrap()
            ),
        ] {
            assert!(args.contains(&config), "{config} in {args:?}");
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("writable_roots") || arg.contains("filesystem")),
            "{args:?}"
        );
        // The options stay before the prompt.
        assert_eq!(&args[args.len() - 2..], ["--", "p"]);
    }

    /// A relative `CODEX_HOME` is read from the main checkout Codex starts
    /// in, not from where `dagq inbox` was typed, and Codex is given that
    /// same absolute path: the file checked is the file Codex reads.
    #[test]
    fn a_relative_codex_home_is_checked_and_given_from_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("repo");
        let typed_in = dir.path().join("repo-worktree").join("sub");
        let at_checkout = checkout.join("codex-home");
        std::fs::create_dir_all(&at_checkout).unwrap();
        std::fs::create_dir_all(typed_in.join("codex-home")).unwrap();
        std::fs::write(
            at_checkout.join("config.toml"),
            "[tui]\nscreen_reader_detection_done = true\n",
        )
        .unwrap();
        // Where it was typed, the same relative home has no setting.
        std::fs::write(typed_in.join("codex-home").join("config.toml"), "").unwrap();
        let relative = || Some(OsString::from("codex-home"));
        let file = user_config_file(relative(), None, &checkout).unwrap();
        assert_eq!(file, at_checkout.join("config.toml"));
        assert_eq!(check_user_config(Some(&file)), Ok(()));
        let typed = user_config_file(relative(), None, &typed_in).unwrap();
        assert!(check_user_config(Some(&typed)).is_err());

        let home = launch_codex_home(relative(), &checkout).unwrap();
        assert_eq!(home, at_checkout);
        let command = inbox_command(
            Path::new("/bin/codex"),
            Path::new("/bin/dagq"),
            &checkout,
            Some(&home),
            None,
            "p",
        )
        .unwrap();
        let given: Vec<_> = command
            .get_envs()
            .filter(|(key, _)| *key == "CODEX_HOME")
            .map(|(_, value)| value.map(PathBuf::from))
            .collect();
        assert_eq!(given, [Some(at_checkout.clone())]);
        // An absolute one stays as it is; an empty one is none.
        assert_eq!(
            launch_codex_home(Some("/abs/home".into()), &checkout),
            Some(PathBuf::from("/abs/home"))
        );
        assert_eq!(launch_codex_home(Some(OsString::new()), &checkout), None);
        assert_eq!(
            user_config_file(None, Some("me".into()), &checkout).unwrap(),
            checkout.join("me/.codex/config.toml")
        );
    }

    #[test]
    fn the_screen_reader_setting_is_found_in_each_form() {
        assert!(has_screen_reader_field(
            "model = \"x\"\n[tui]\nscreen_reader_detection_done = true\n"
        ));
        assert!(has_screen_reader_field(
            "[tui]\n  screen_reader_detection_done=false # set\n"
        ));
        assert!(has_screen_reader_field(
            "tui.screen_reader_detection_done = true\n[projects]\n"
        ));
        assert!(has_screen_reader_field(
            "tui = { screen_reader_detection_done = true }\n"
        ));
        assert!(has_screen_reader_field(
            "[\"tui\"]\n\"screen_reader_detection_done\" = true\n"
        ));
        assert!(!has_screen_reader_field(""));
        assert!(!has_screen_reader_field(
            "[tui]\nanimations = false\n[other]\nscreen_reader_detection_done = true\n"
        ));
        assert!(!has_screen_reader_field(
            "[tui.model_availability_nux]\nscreen_reader_detection_done = 1\n"
        ));
        assert!(!has_screen_reader_field(
            "# screen_reader_detection_done = true\n[tui]\n"
        ));
        // A quoted key holding the dot is a key of the top, and a comment
        // naming the setting is no setting.
        assert!(!has_screen_reader_field(
            "\"tui.screen_reader_detection_done\" = true\n"
        ));
        assert!(!has_screen_reader_field(
            "tui = { animations = false } # screen_reader_detection_done\n"
        ));
        assert!(has_screen_reader_field(
            "[tui] # the TUI\nscreen_reader_detection_done = true # done\n"
        ));
        assert!(has_screen_reader_field(
            "[ tui ]\nanimations = false\n\"screen_reader_detection_done\" = true\n"
        ));
        assert!(has_screen_reader_field(
            "[projects.\"/a.b\"]\ntrust_level = \"trusted\"\n[tui]\nx = [1,\n  2]\nscreen_reader_detection_done = false\n"
        ));
        assert!(has_screen_reader_field(
            "tui = { animations = false, screen_reader_detection_done = true }\n"
        ));
        // A `[tui]` written inside a string, a multi-line one or one in an
        // array, is no table.
        for inside in [
            "developer_instructions = \"\"\"\n[tui]\nscreen_reader_detection_done = true\n\"\"\"\n",
            "developer_instructions = '''\n[tui]\nscreen_reader_detection_done = true\n'''\n",
            "notes = [\n  \"\"\"\n[tui]\nscreen_reader_detection_done = true\n\"\"\",\n]\n",
            "a = \"[tui] screen_reader_detection_done = true\"\n",
            "[tui]\nx = { screen_reader_detection_done = true }\n",
            "[tui.screen_reader_detection_done]\nvalue = true\n",
        ] {
            assert!(!has_screen_reader_field(inside), "{inside:?}");
        }
        // After such a string the file is still read.
        assert!(has_screen_reader_field(
            "developer_instructions = \"\"\"\nno \\\"\"\" end\n\"\"\"\n[tui]\nscreen_reader_detection_done = true\n"
        ));
        // A quoted key is compared with its escapes decoded, as Codex
        // reads it.
        for escaped in [
            "[\"\\u0074ui\"]\nscreen_reader_detection_done = true\n",
            "\"\\U00000074ui\".screen_reader_detection_done = true\n",
            "[tui]\n\"screen\\u005freader_detection_done\" = true\n",
            "tui = { \"screen_reader_detection_\\u0064one\" = true }\n",
        ] {
            assert!(has_screen_reader_field(escaped), "{escaped:?}");
        }
        assert!(!has_screen_reader_field(
            "[\"t\\u0075i2\"]\nscreen_reader_detection_done = true\n"
        ));
        // An escape TOML does not have is no TOML.
        assert!(!has_screen_reader_field(
            "[tui]\n\"screen_reader_detection_\\qone\" = true\nscreen_reader_detection_done = true\n"
        ));
        // A file it cannot read through counts as missing.
        assert!(!has_screen_reader_field(
            "a = \"\"\"never closed\n[tui]\nscreen_reader_detection_done = true\n"
        ));
    }

    /// A Codex home whose `config.toml` lacks the setting, has none, or
    /// cannot be read keeps the inbox closed with why, where the file is
    /// and the lines to add, and the file is left as it was; with the
    /// setting the inbox goes on.
    #[test]
    fn the_inbox_opens_only_when_the_persons_config_has_the_screen_reader_setting() {
        let home = tempfile::tempdir().unwrap();
        let codex_home = home.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        let cwd = Path::new("/elsewhere");
        let file = user_config_file(
            Some(codex_home.clone().into()),
            Some(home.path().into()),
            cwd,
        )
        .unwrap();
        assert_eq!(file, codex_home.join("config.toml"));
        // `$HOME/.codex` without `CODEX_HOME`, or with an empty one.
        assert_eq!(
            user_config_file(Some(OsString::new()), Some(home.path().into()), cwd).unwrap(),
            home.path().join(".codex/config.toml")
        );
        assert_eq!(user_config_file(None, None, cwd), None);

        let refused = |error: String, why: &str| {
            assert!(
                error.starts_with("the Codex inbox was not opened: ")
                    && error.contains(why)
                    && error.ends_with(SCREEN_READER_LINES),
                "{error}"
            );
        };
        refused(
            check_user_config(Some(&file)).unwrap_err(),
            "does not exist",
        );
        assert!(!file.exists(), "a missing file is not created");
        refused(
            check_user_config(None).unwrap_err(),
            "neither CODEX_HOME nor HOME",
        );

        let without = "model = \"gpt-6.1-sol\"\n[tui]\nanimations = false\n";
        std::fs::write(&file, without).unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        let error = check_user_config(Some(&file)).unwrap_err();
        refused(error.clone(), "has no tui.screen_reader_detection_done");
        assert!(error.contains(&file.display().to_string()), "{error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), without);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );

        std::fs::write(&file, b"\xff\xfe").unwrap();
        refused(check_user_config(Some(&file)).unwrap_err(), "not UTF-8");
        // A directory in the file's place cannot be read as one.
        let unreadable = codex_home.join("dir");
        std::fs::create_dir(&unreadable).unwrap();
        refused(
            check_user_config(Some(&unreadable)).unwrap_err(),
            "could not be read",
        );

        let with = format!("{without}screen_reader_detection_done = true\n");
        std::fs::write(&file, &with).unwrap();
        assert_eq!(check_user_config(Some(&file)), Ok(()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), with);
    }
}
