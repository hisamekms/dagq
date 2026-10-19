# up and down: what to do with their outputs

Read this when `up` or `down` returns something the `dagq-recover` skill (section 5) does not explain. Both are typed from the inbox (or by the person in a terminal without `DAGQ_ROLE`), on the person's word. The flags are in `"$DAGQ" up --help` and `"$DAGQ" down --help`.

## up

`up` needs an initialized queue (`init` from the `dagq` skill first), keeps one supervisor resident under launchd and ends with a `doctor` summary to report. It opens neither the inbox (`inbox.outcome` `not_opened`: the person opens it with `dagq inbox` in their own terminal, below) nor a planner; `dagq plan` is refused (a plan is the inbox's request: `skills/dagq-inbox/reference/requests.md`).

- Sessions load the plugin installed in Claude Code. Pass `--plugin-dir PATH` only when the repository's instructions name a plugin checkout; never `$CLAUDE_PLUGIN_ROOT` (a per-version cache an update leaves stale and deletes).
- Pass `--claude` and `--codex` as real paths (`~/.local/bin/claude`, `~/.local/bin/codex`): a bare name a terminal's shim resolves would be fixed on the supervisor. An update of Claude Code or Codex needs no new `up`. A missing `codex` does not stop `up`: Codex tasks start on Claude. `status`'s `supervisors[].providers` shows what was resolved.
- `supervisor.outcome` `reused` touches nothing but `auto_update`, which follows this `up`'s `--auto-update`. To change `--claude`, `--codex` or `--max-waiting` of a supervisor of the same build, `down --wait`, then `up` again.
- `down` closes neither the inbox nor a planner. A workspace an earlier binary opened for a role (`retired_sessions`) is forgotten, not closed: the person closes it in their own terminal without `DAGQ_ROLE`.
- `warnings`, `stray_agents`, `pruned_supervisors`: report them; nothing else failed.
- A preflight that fails (Claude Code, the plugin, `gh` for the CI watch) stops `up` with why and what to do: have the person install or log in (`gh auth login`) in their own terminal, or ask through a planning request for a task that changes the repository's configuration. `doctor`'s `ci_watch` shows what resolved.

### Explicitly run without Claude

On the person's word, drain first, then `up --no-claude`; existing Claude workers must finish before switching, and an old Claude run needing a resume waits for recovery by hand. `dagq inbox` starts Claude Code, so keep an inbox the person opened by hand (`up`'s `inbox.next`). Workers and roles configured for Codex run on Codex and never fall back to Claude. Roles on Claude are off (`provider_disabled` / `handle disabled roles manually` in `status`): handle their reviews, plan reviews and recovery with this skill's manual procedures. The flag implies no approval or bypass. Do not run `observe` in their place. A live supervisor with another policy is drained before changing it.

### A supervisor of another build

Replace the binary with `install` (`reference/update.md`), never `cp`. A supervisor of another build is handed over in place (`"handoff": true`): runs go on. One from before the handoff is drained instead: tell the person what it waits for and never kill it; `up --no-wait` refuses while it still leases a run.

A command that stops with "run `dagq migrate`": `dagq migrate --check` lists the migrations; `up` and `install` apply the compatible ones, a breaking one needs `install --allow-breaking` (`reference/update.md`); tell the person first. `unsupported queue schema version`: use the newer binary.

A supervisor alive but no longer heartbeating is not replaced (`status` shows it `stale`): the person stops it with `down --force`.

### Runs waiting for a person

A run whose session waits on an ask leaves its slot, up to `--max-waiting`, and keeps its lease and session. So a drain (`down`, `down --wait`, `install --allow-breaking`) waits until each such ask is answered: show the person the open asks (`status`'s `waiting`) before a drain. A handoff does not wait for them.

### A supervisor of the retired mode

One an earlier binary started outside launchd (`mode` `in_cmux` in `status`) keeps working and takes handoffs, but nothing restarts it. Move it to launchd once: `down --wait`, then `up`, keeping your usual flags (for example `--auto-update`). `down` never closes the terminal it ran in (`supervisor_workspaces` `left_open`, with `--wait` too): the person closes it.

### Two supervisors of one queue

`restart supervisor` with `supervisor_duplicate` (or `duplicate_supervisors` in `status` / `doctor`): `up` alone reuses one and does not fix it. On the person's word, `down --wait`, then `up`. If an agent is still loaded, run the `launchctl bootout` its `fix` names and remove its plist. A handoff's two rows under one pid are not a duplicate.

### Open the inbox again (the guardrail)

`inbox_guardrail` `false` (`status`, `doctor`) means the inbox was opened without its settings; a `claude` typed again in the inbox's terminal has none either and is not seen. Reopen it the same way after a plugin update, or once to move an inbox an earlier binary opened in a cmux workspace. On the person's word:

1. The inbox writes its handoff first (open items, the watch's cursor, what the person said to keep), where the repository's instructions keep it.
2. The person ends that inbox (it cannot end itself).
3. The person closes that inbox's terminal or workspace if they no longer need it, and runs `dagq inbox` in a terminal without `DAGQ_ROLE` (it is refused inside an inbox): the new inbox has the settings, reads the handoff and starts its watch. Its flags: `dagq inbox --help`.

## down

- Plain `down` ends the day's work: the supervisor stops claiming, drains its runs and exits.
- `--wait` blocks until it is gone. Use it when the next step needs that (changing flags, moving a supervisor of the retired mode, shutting down).
- `--force` loses the active runs (handled afterwards with this skill): only on the person's explicit word.

## Logs

`"$DAGQ" locate` prints `log_dir`. Each process writes its own JSON Lines file there (`jq 'select(.fields.run_id == "<run-id>")'` picks one run); launchd's output is `launchd.log`. Read them when a supervisor is missing from `status`, `up` reports that none registered, or a run failed for a reason `show` does not explain. Deleting old files is the person's call.
