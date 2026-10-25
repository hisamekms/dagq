# The inbox on Claude Code and on Codex

Read this when the inbox runs on Codex, or when its start, its hooks or its guardrail are in question. The person picks the provider and model when opening it (`dagq inbox --help`); `status`'s `inbox_guardrail` shows what the newest inbox opened on, and `doctor`'s `roles` the configured one. The skill's steps are the same on both; what differs:

| | Claude Code | Codex |
| --- | --- | --- |
| The dagq CLI | `"${CLAUDE_PLUGIN_ROOT}/bin/dagq"`, kept in `$DAGQ` (`"$DAGQ" --resolve`) | the absolute path of the `dagq` that opened the inbox, which the session's instructions and its start hook name. Shell state does not last from one command to the next, so write that path out wherever the skill says `"$DAGQ"`. `${CLAUDE_PLUGIN_ROOT}` is the plugin directory two levels above this skill's `SKILL.md`. |
| Instructions | this skill, loaded by the plugin | the session's instructions name this skill's `SKILL.md`: read it and its references |
| The watch | `watch --role inbox --until-attention` in the background | `watch --role inbox` in the foreground, bounded by its timeout, run again each time it returns (`watch.md`) |
| Showing an ask | `AskUserQuestion` | a numbered list, the person's reply as text (`asks.md`) |
| Hooks | the plugin's | given by `dagq inbox` with the start, trusted for that start only |
| Guardrail | the inbox's settings (`permissions.deny`) | the PreToolUse hook |

## Coming back after a compaction, `/clear` or a new start

- On both, a SessionStart hook prints `status --role inbox` at the start, after a compaction and after `/clear`; start again from step 1. On Codex it also comes after an automatic compaction (source `compact`, the same session going on), and it names the watch to run first, from the status's cursor.
- Claude: look at the session's background tasks first; a watch still running brings its own notice (`watch.md`). Codex has none: no watch outlives a turn, so run the watch again.
- To start the conversation over, use `/clear`. On Codex never use `/new`: it moves the conversation into a new worktree of Codex's own, away from the main checkout the inbox runs in. Never resume an earlier Codex session by hand either: it starts without the inbox's hooks and instructions. To restart, the person ends the inbox and runs `dagq inbox` again (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/up-down.md`, "Open the inbox again").

## Hooks and the guardrail

The guardrail refuses the commands the inbox may not run. It is advice; the CLI's own authorization is the check. On both it matches the head of a command only: a form wrapped after `&&`, `;` or a pipe, or inside `sh -c`, passes it and the CLI refuses it with `authorization_denied`. Either refusal: do not try another form; tell the person what you wanted to run and why, and leave it to them.

- Claude: the guardrail is the settings `dagq inbox` writes for the inbox. `inbox_guardrail.guardrail` `false` means the inbox opened without them: reopen it (`status.md`).
- Codex: the guardrail is a PreToolUse hook. A refused command returns `Command blocked by PreToolUse hook: <reason>. Command: <cmd>` to you instead of running. `inbox_guardrail.guardrail_by` is `pre_tool_use`, and `reached` turns `true` once the hook is seen running (at the first shell command after the start). `reached` still `false` after you ran shell commands: the hook does not run, so tell the person and have them reopen the inbox.

### Codex's hook trust screen

`dagq inbox` trusts the hooks it gives for that start only, never every hook, and writes no trust. Codex still shows its trust screen at the start when one of dagq's hooks no longer matches what dagq trusted (after a Codex update, say), and also for hooks dagq does not give (a project's `.codex/`, the person's own). The screen alone does not tell which.

- The person does not choose "Trust all": it writes the trust into their Codex settings. They choose "Continue without trusting".
- Until the list under review ("Review hooks") or each hook's state shows that none of dagq's hooks (they run `dagq codex-inbox-hook <event>`) is among them, take one of dagq's hooks as not running: the person opens the inbox on Claude instead, or you report it.

## The person's Codex settings

Neither dagq nor the inbox writes the person's Codex settings (`config.toml` and `auth.json` under `CODEX_HOME` or `~/.codex`) or `~/.claude`.

- When Codex's TUI would add a setting to the person's `config.toml` by itself, `dagq inbox` does not open Codex: it prints why, the file and the lines to add. The person adds those lines themselves and runs `dagq inbox` again.
- In Codex's TUI, choosing "Trust all" on the trust screen, or the newer model the notice after a login offers, writes to the person's settings. Choose neither, or choose knowing that it is written there.
- A Codex that does not run or is not logged in opens no inbox: `dagq inbox` says why and, unless Claude is forbidden on the queue, how to open it on Claude, never switching by itself. A Codex that stops working after the start (a usage limit) changes nothing in the queue: the person reopens the inbox.
