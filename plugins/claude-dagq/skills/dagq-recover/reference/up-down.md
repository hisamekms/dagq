# up and down: what to do with their outputs

Read this when `up` or `down` returns something the `dagq-recover` skill (section 5) does not explain. Both are typed from the inbox (or by the person in a terminal without `DAGQ_ROLE`), on the person's word. The flags are in `"$DAGQ" up --help` and `"$DAGQ" down --help`.

## up

`up` needs an initialized queue (`init` from the `dagq` skill first), keeps one supervisor resident under launchd and ends with a `doctor` summary to report. It opens neither the inbox (the person opens it with `dagq inbox` in their own terminal, below) nor a planner; `dagq plan` is refused (a plan is the inbox's request: `skills/dagq-inbox/reference/requests.md`).

- Sessions load the plugin installed in Claude Code. Pass `--plugin-dir PATH` only when the repository's instructions name a plugin checkout; never `$CLAUDE_PLUGIN_ROOT` (a per-version cache an update leaves stale and deletes).
- Pass `--claude` and `--codex` as real paths (`~/.local/bin/claude`, `~/.local/bin/codex`): a bare name a terminal's shim resolves would be fixed on the supervisor. An update of Claude Code or Codex needs no new `up`. `status` shows what was resolved.
- An `up` that reuses a live supervisor changes only its automatic updates. To change its other flags, `down --wait`, then `up` again.
- `down` closes neither the inbox nor a planner. A workspace an earlier binary opened for a role is closed by the person in their own terminal without `DAGQ_ROLE`.
- Warnings in the output: report them; nothing else failed.
- A preflight that fails stops `up` with why and what to do: have the person install or log in (`gh auth login`) in their own terminal, or ask through a planning request for a task that changes the repository's configuration.

### Explicitly run without Claude

On the person's word, drain first, then `up --no-claude`; existing Claude workers must finish before switching. While a supervisor runs under `--no-claude`, `dagq inbox` opens no Claude inbox (only a Codex one that can be used, else it says why), so keep the inbox already open. Roles on Claude are off (`handle disabled roles manually` in `status`): handle their reviews, plan reviews and recovery with this skill's manual procedures. The flag implies no approval or bypass. Do not run `observe` in their place.

### A supervisor of another build

Replace the binary with `install` (`reference/update.md`), never `cp`. A supervisor of another build is handed over in place: runs go on. One that is drained instead: tell the person what it waits for and never kill it.

A command that stops with "run `dagq migrate`": `dagq migrate --check` lists the migrations; `up` and `install` apply the compatible ones, a breaking one needs `install --allow-breaking` (`reference/update.md`); tell the person first. `unsupported queue schema version`: use the newer binary.

A supervisor alive but no longer heartbeating is not replaced (`status` shows it stale): with no other live supervisor, the person stops it with `down --force`; beside a live one, see "Two supervisors of one queue". A registration whose supervisor died needs nothing.

### Runs waiting for a person

A drain (`down`, `down --wait`, `install --allow-breaking`) waits until each run waiting on an ask has its answer: show the person the open asks before a drain. A handoff does not wait for them.

### A supervisor of the retired mode

One an earlier binary started outside launchd (`status` names its mode) keeps working, but nothing restarts it. Move it to launchd once: `down --wait`, then `up`, keeping your usual flags (for example `--auto-update`). The person closes the terminal it ran in.

### Two supervisors of one queue

When `status` or `doctor` reports duplicate supervisors, `up` alone reuses one and does not fix it. On the person's word, `down --wait`, then `up`. If an agent is still loaded, run the `launchctl bootout` the output names and remove its plist. A handoff's two rows under one pid are not a duplicate.

A stale registration beside a live supervisor is not a restart: `up` would only reuse the live one. A dead one needs nothing. A hung one shows `stop the stale supervisor's process`: on the person's word, check the pid is still a `dagq supervise` of this queue (`ps -p PID -o command`; if the pid was reused by another program, run `up`), then stop it (`kill PID`; not `down`, which stops the live one).

### Open the inbox again (the guardrail)

When `status` or `doctor` says the inbox's guardrail is off, the inbox was opened without its settings; a `claude` typed again in the inbox's terminal has none either and is not seen. A Codex inbox whose guardrail hook never ran (`reached` still `false`), or one whose start left a dagq hook untrusted, is reopened the same way (`skills/dagq-inbox/reference/claude-and-codex.md`); never by resuming the Codex session by hand. Reopen it the same way after a plugin update, or once to move an inbox an earlier binary opened in a cmux workspace. On the person's word:

1. The inbox writes its handoff first (open items, the watch's cursor, what the person said to keep), where the repository's instructions keep it.
2. The person ends that inbox (it cannot end itself).
3. The person closes that inbox's terminal or workspace if they no longer need it, and runs `dagq inbox` in a terminal without `DAGQ_ROLE` (it is refused inside an inbox): the new inbox has the settings, reads the handoff and starts its watch. Its flags: `dagq inbox --help`.

## down

- Plain `down` ends the day's work: the supervisor stops claiming, drains its runs and exits.
- `--wait` blocks until it is gone. Use it when the next step needs that (changing flags, moving a supervisor of the retired mode, shutting down).
- `--force` loses the active runs (handled afterwards with this skill): only on the person's explicit word.

## Logs

`"$DAGQ" locate` prints the log directory. Read them when a supervisor is missing from `status`, `up` reports that none registered, or a run failed for a reason `show` does not explain. Deleting old files is the person's call.
