//! What the hooks of a Codex inbox answer (ADR-t1181-1 decision 2): the
//! same roles as the Claude inbox's plugin hooks, decided here on the
//! hook's input and the queue's status, and run by `dagq
//! codex-inbox-hook <event>` ([`crate::infrastructure::codex_inbox`]),
//! which `dagq inbox` names in the Codex inbox's launch arguments.
//!
//! - `session-start`: the inbox's `status --role inbox` with the first
//!   move (the inbox's watch in the foreground), whatever the source
//!   (`startup`, `resume`, `clear`, `compact`), so the inbox re-orients
//!   itself after a start, a compaction or `/clear`; and the span's open.
//! - `session-end`: the span's close.
//! - `pre-compact`, `post-compact`: the compaction's mark on the span
//!   (its `trigger`, `auto` or `manual`); the span goes on.
//! - `stop`: keeps a turn from ending without the inbox's watch.
//! - `pre-tool-use`: the guardrail ([`guard`]), and the record that it
//!   ran, once a session.
//!
//! The input is the hook's stdin JSON as Codex 0.160.0 gives it (the same
//! names as Claude Code's): `session_id`, `transcript_path`, `cwd`,
//! `hook_event_name`, `model` (but SessionEnd's), and per event `source`
//! (SessionStart), `reason` (SessionEnd), `trigger` and `turn_id`
//! (PreCompact, PostCompact; measured with 0.160.0 after an automatic
//! compaction: `auto`, then a SessionStart of `source` `compact` with the
//! same `session_id`), `stop_hook_active` (Stop) or `tool_name` and
//! `tool_input.command`
//! (PreToolUse; a shell command arrives as `tool_name` `Bash` with the
//! command string as the model gave it, not wrapped in a shell).

use serde_json::{Value, json};

use super::execution::deny_forms;
use crate::domain::ActorRole;

/// A hook dagq gives a Codex inbox, by the argument of `dagq
/// codex-inbox-hook` that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxHook {
    SessionStart,
    SessionEnd,
    Stop,
    PreToolUse,
    PreCompact,
    PostCompact,
}

impl InboxHook {
    pub const ALL: [Self; 6] = [
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::PreToolUse,
        Self::PreCompact,
        Self::PostCompact,
    ];

    /// The argument of `dagq codex-inbox-hook` that runs this hook.
    pub fn argument(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::SessionEnd => "session-end",
            Self::Stop => "stop",
            Self::PreToolUse => "pre-tool-use",
            Self::PreCompact => "pre-compact",
            Self::PostCompact => "post-compact",
        }
    }

    pub fn from_argument(argument: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|hook| hook.argument() == argument)
    }
}

/// The inbox's watch as the Codex inbox runs it, in the foreground of its
/// turn: `watch --role inbox` from `cursor` (from now without one), which
/// returns within its default timeout, so the turn gets it back whether
/// or not anything came.
pub fn foreground_watch(launcher: &str, cursor: Option<i64>) -> String {
    let after = cursor.map_or(String::new(), |cursor| format!(" --after {cursor}"));
    format!("{} watch --role inbox{after}", shell_word(launcher))
}

/// `text` as one word of a shell command: as it is when it has only
/// characters no shell treats specially, else single-quoted (a `'` in it
/// closed, escaped and opened again), so no `$`, backquote or quote in a
/// path is read by the shell the command is run in.
pub fn shell_word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,".contains(c))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// The top-level `cursor` of a status.
fn cursor(status: &Value) -> Option<i64> {
    status["cursor"].as_i64()
}

/// What the SessionStart hook prints, which Codex adds to the context:
/// with `status` read (the text of `status --role inbox`), the first move
/// (run [`foreground_watch`] from its cursor and run it again from the
/// cursor each one returns), one line naming the role and its
/// instructions, and the status; else one line saying why it could not be
/// read. The same for every source, a compaction's included.
pub fn session_start_context(launcher: &str, status: Result<&str, &str>) -> String {
    match status {
        Ok(text) => {
            let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
            let watch = foreground_watch(launcher, cursor(&parsed));
            format!(
                "First move, before anything else: run the inbox's watch in the foreground, the one command {watch} (it returns within its timeout), handle what it returns, and run it again from the cursor it returns; keep doing so whenever the person is not talking to you.\n\
                 This session is the dagq inbox (DAGQ_ROLE=inbox); follow the dagq-inbox skill as this session's instructions say. The queue status for this role (dagq status --role inbox); when its language.instruction is set, write for people as it says:\n\
                 {text}\n"
            )
        }
        Err(error) => format!(
            "dagq status failed: {}\n",
            error.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
    }
}

/// What the Stop hook answers: `None` lets the turn end, `Some` is the
/// control output `{"decision": "block", "reason": …}` that keeps the
/// session going with the reason. It blocks when `status` (of `status
/// --role inbox`) counts no watch running (`inbox_watcher.watching` is
/// 0), at most once in a row: an `input` with `stop_hook_active` true (the
/// session already went on because of a Stop hook) lets the turn end. A
/// status without the count lets it end too.
pub fn stop_decision(input: &Value, status: &Value, launcher: &str) -> Option<Value> {
    if input["stop_hook_active"] == true {
        return None;
    }
    if status["inbox_watcher"]["watching"].as_i64()? > 0 {
        return None;
    }
    let watch = foreground_watch(launcher, cursor(status));
    Some(json!({
        "decision": "block",
        "reason": format!(
            "No dagq inbox watch is running (status --role inbox: inbox_watcher.watching is 0), so asks and attention would not reach the person. If a watch has just returned output you have not handled, handle it first. Then, before ending the turn, run the inbox's watch in the foreground from its cursor, as this one command: {watch}."
        ),
    }))
}

/// The guardrail of the Codex inbox, on a PreToolUse hook's `input`: the
/// control output that refuses the tool call, or `None` to let it run. A
/// call of the shell tool (`tool_name` `Bash`) whose `tool_input.command`
/// starts with a form of the inbox's guardrail ([`deny_forms`] of
/// [`ActorRole::Inbox`], as Claude's `permissions.deny`) is refused; its
/// head only, so a wrapped form (after `&&`, `;`, a pipe, inside `sh -c`)
/// passes here and the CLI's own check refuses it. Every other tool, and
/// an input without a command, passes.
///
/// The refusal is Codex's `{"hookSpecificOutput": {"hookEventName":
/// "PreToolUse", "permissionDecision": "deny",
/// "permissionDecisionReason": …}}` on stdout with exit 0, which blocks
/// the call and gives the model `Command blocked by PreToolUse hook:
/// <reason>`.
pub fn guard(input: &Value) -> Option<Value> {
    if input["tool_name"] != "Bash" {
        return None;
    }
    let command = input["tool_input"]["command"].as_str()?;
    let form = deny_forms(ActorRole::Inbox)
        .into_iter()
        .find(|form| form.matches_head(command))?;
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!(
                "dagq inbox guardrail: {} is denied to the inbox role. Do not retry it in another form; tell the person what you wanted to run and why, and leave it to them.",
                form.claude_rule()
            ),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(command: &str) -> Value {
        json!({
            "session_id": "01a16f61-0000",
            "hook_event_name": "PreToolUse",
            "model": "gpt-6.1-sol",
            "permission_mode": "default",
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "tool_use_id": "exec-a005b097",
        })
    }

    fn denied(command: &str) -> bool {
        match guard(&bash(command)) {
            Some(output) => {
                let specific = &output["hookSpecificOutput"];
                assert_eq!(specific["hookEventName"], "PreToolUse");
                assert_eq!(specific["permissionDecision"], "deny");
                assert!(
                    specific["permissionDecisionReason"]
                        .as_str()
                        .unwrap()
                        .contains("inbox guardrail"),
                    "{output}"
                );
                true
            }
            None => false,
        }
    }

    /// The forms (a) to (h) the guardrail was measured with: the heads of
    /// a denied command and of the variables that name the actor are
    /// refused, the wrapped forms pass to the CLI's own check, and a
    /// command the inbox may run passes.
    #[test]
    fn the_guard_refuses_the_denied_heads_and_passes_wrapped_forms() {
        // (a), (f), (g).
        assert!(denied("dagq request decline"));
        assert!(denied("DAGQ_ROLE=worker dagq status"));
        assert!(denied("export DAGQ_ROLE=worker; dagq status"));
        // (b) to (e): not the head.
        assert!(!denied("true && dagq request decline"));
        assert!(!denied("true; dagq request decline"));
        assert!(!denied("echo | dagq request decline"));
        assert!(!denied("sh -c 'dagq request decline'"));
        // (h).
        assert!(!denied("dagq status"));
        // The rest of the inbox's denials, and a whole word only.
        assert!(denied("  dagq replan propose --run r1 --reason r"));
        assert!(denied("unset DAGQ_ACTOR_ID"));
        assert!(denied("env -u DAGQ_RUN_ID dagq status"));
        assert!(!denied("dagq request declined"));
        assert!(!denied("dagq request add --text t"));
        assert!(!denied("rg 'dagq request decline'"));
    }

    /// The guard refuses exactly what the inbox's Claude rules refuse:
    /// both are made from the same forms of the inbox's policy.
    #[test]
    fn the_guard_and_the_claude_rules_come_from_the_same_forms() {
        let rules = crate::application::execution::permission_deny(ActorRole::Inbox);
        let forms = deny_forms(ActorRole::Inbox);
        assert_eq!(rules.len(), forms.len());
        for form in forms {
            assert!(rules.contains(&form.claude_rule()));
            let head = match &form {
                crate::application::execution::DenyForm::Command(command) => command.clone(),
                crate::application::execution::DenyForm::Prefix(text) => format!("{text}x"),
            };
            assert!(denied(&head), "{head}");
        }
        assert!(
            rules.contains(&"Bash(dagq request decline:*)".to_owned())
                && rules.contains(&"Bash(dagq replan propose:*)".to_owned()),
            "{rules:?}"
        );
    }

    #[test]
    fn the_guard_passes_other_tools_and_inputs_without_a_command() {
        let mut other = bash("dagq request decline");
        other["tool_name"] = "apply_patch".into();
        assert_eq!(guard(&other), None);
        assert_eq!(guard(&json!({"tool_name": "Bash", "tool_input": {}})), None);
        assert_eq!(guard(&Value::Null), None);
    }

    /// The SessionStart hook's context is the same for every source, a
    /// compaction's included (the input of a SessionStart after an
    /// automatic compaction, as Codex gives it), with the foreground watch
    /// from the status's cursor first.
    #[test]
    fn the_session_start_context_re_orients_the_inbox_after_a_compaction() {
        let compact: Value = serde_json::from_str(
            r#"{"session_id":"01a16fd0-ea53-77d3-8a8a-46698a8bd53e","transcript_path":"/codex/sessions/rollout.jsonl","cwd":"/repo","hook_event_name":"SessionStart","model":"gpt-6.1-sol","permission_mode":"default","source":"compact"}"#,
        )
        .unwrap();
        assert_eq!(compact["source"], "compact");
        let status = "{\n  \"cursor\": 42,\n  \"inbox_watcher\": {\n    \"watching\": 0\n  }\n}";
        let context = session_start_context("/bin/dagq", Ok(status));
        assert!(
            context.starts_with(
                "First move, before anything else: run the inbox's watch in the foreground, the one command /bin/dagq watch --role inbox --after 42"
            ),
            "{context}"
        );
        assert!(context.contains("dagq status --role inbox"), "{context}");
        assert!(context.ends_with(&format!("{status}\n")), "{context}");
        // Without a cursor the watch starts from now.
        assert!(session_start_context("dagq", Ok("{}")).contains(" dagq watch --role inbox (it"));
        assert_eq!(
            session_start_context("dagq", Err("no\nqueue  here\n")),
            "dagq status failed: no queue here\n"
        );
    }

    #[test]
    fn the_stop_hook_blocks_once_a_turn_that_ends_without_a_watch() {
        let none = json!({"cursor": 7, "inbox_watcher": {"watching": 0}});
        let input = json!({"hook_event_name": "Stop", "stop_hook_active": false});
        let block = stop_decision(&input, &none, "/bin/dagq").unwrap();
        assert_eq!(block["decision"], "block");
        assert!(
            block["reason"]
                .as_str()
                .unwrap()
                .contains("/bin/dagq watch --role inbox --after 7."),
            "{block}"
        );
        // Once in a row.
        let again = json!({"stop_hook_active": true});
        assert_eq!(stop_decision(&again, &none, "dagq"), None);
        // A watch running, or a status without the count.
        let one = json!({"cursor": 7, "inbox_watcher": {"watching": 1}});
        assert_eq!(stop_decision(&input, &one, "dagq"), None);
        assert_eq!(stop_decision(&input, &json!({"cursor": 7}), "dagq"), None);
    }

    /// A launcher path the shell would read (a command substitution, a
    /// backquote, a quote, a space) is one single-quoted word in the watch
    /// the instructions and the hooks tell the inbox to run.
    #[test]
    fn the_watch_command_quotes_a_launcher_the_shell_would_read() {
        assert_eq!(
            foreground_watch("/Users/me/.local/bin/dagq", Some(3)),
            "/Users/me/.local/bin/dagq watch --role inbox --after 3"
        );
        assert_eq!(
            foreground_watch("/opt/$(id)/dagq", None),
            "'/opt/$(id)/dagq' watch --role inbox"
        );
        assert_eq!(shell_word("/a`id`/\"b\" c"), "'/a`id`/\"b\" c'");
        assert_eq!(shell_word("/it's/dagq"), r"'/it'\''s/dagq'");
        assert_eq!(shell_word(""), "''");
        let block = stop_decision(
            &json!({}),
            &json!({"inbox_watcher": {"watching": 0}}),
            "/opt/$(id)/dagq",
        )
        .unwrap();
        assert!(
            block["reason"]
                .as_str()
                .unwrap()
                .contains("'/opt/$(id)/dagq' watch"),
            "{block}"
        );
        assert!(
            session_start_context("/opt/$(id)/dagq", Ok("{}")).contains("'/opt/$(id)/dagq' watch")
        );
    }

    #[test]
    fn each_hook_is_named_by_its_argument() {
        for hook in InboxHook::ALL {
            assert_eq!(InboxHook::from_argument(hook.argument()), Some(hook));
        }
        assert_eq!(InboxHook::from_argument("open"), None);
    }
}
