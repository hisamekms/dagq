# up and down: what to do with their outputs

Read this when `up` or `down` returns something the `dagq-recover` skill (section 5) does not explain. Both are typed from the inbox (or by the person in a terminal without `DAGQ_ROLE`), on the person's word. The flags are in `"$DAGQ" up --help` and `"$DAGQ" down --help`.

## up

`up` needs an initialized queue (`init` from the `dagq` skill first), keeps one supervisor resident under launchd and ends with a `doctor` summary to report. It opens neither the inbox (the person opens it with `dagq inbox` in their own terminal, below) nor a planner; `dagq plan` is refused (a plan is the inbox's request: `skills/dagq-inbox/reference/requests.md`).

- Sessions load the plugin installed in Claude Code. Pass `--plugin-dir PATH` only when the repository's instructions name a plugin checkout; never `$CLAUDE_PLUGIN_ROOT` (a per-version cache an update leaves stale and deletes).
- Pass `--claude` and `--codex` as real paths (`~/.local/bin/claude`, `~/.local/bin/codex`): a bare name a terminal's shim resolves would be fixed on the supervisor. An update of Claude Code or Codex needs no new `up`. `status` shows what was resolved.
- An `up` that reuses a live supervisor changes only its automatic updates. To change its other flags, `down --wait`, then `up` again.
- `down` closes neither the inbox nor a planner. A workspace an earlier binary opened for a role is closed by the person in their own terminal without `DAGQ_ROLE`.
- Warnings in the output: report them; nothing else failed.
- Afterwards, `status` should answer and show the supervisor: an `unreachable` one means the queue's service still does not answer (`dagq service status` and `service.log`). Then the inbox starts its watch again from its last cursor.
- A preflight that fails stops `up` with why and what to do: have the person install or log in (`gh auth login`) in their own terminal, or ask through a planning request for a task that changes the repository's configuration.

### Explicitly run without Claude

On the person's word, drain the current supervisor and its Claude workers before switching:

1. While the current supervisor can still use Claude, read `dagq status` for the [recorded waits that survive a stop](#runs-waiting-for-a-person). For each entry in `waiting`, match its `task_id` to `held[].task_id` and its `run_id` to `asks[].run_id`. A hold or an unanswered ask for that run blocks `state: waiting`. A hold blocks `state: returning`. The wait can be in a running session, a leased `awaiting_integration` revise, or an already started `needs_session` resume. To identify Claude workers, open `dagq show TASK --full` for the task and match the run's `id` to the `run_id` in `status`. Read `actual_provider` (`claude`) from that run in `show`.
2. Show the person the blocking asks and holds. Answer the asks in the person's own words and use `release` for the holds as they direct. Resolve every blocker for that wait, including both the hold and the unanswered asks when they coexist. Read `status` again and verify those waits are free of the blockers above before stopping the current supervisor.
3. Run `dagq down --wait`, then `dagq up --no-claude`. If you need to prevent all Claude worker launches, apply the check below between these commands.

**When Claude launches must be prevented.** An inherited wrapper keeps its original launch configuration and never receives the new supervisor's `--no-claude` flag. On each new turn it can launch a Claude process and can start a new Claude conversation. This is a known gap in enforcement of the launch ban.

After `down --wait`, the last supervisor and the queue service have stopped, so reads fail as `unreachable`. Run `dagq service start` to start just the queue service and confirm it answers with `dagq service status`. Leave the service running for the next `up` to use. Read `dagq status` and match each relevant entry in `runs` by `run_id` to its run in `dagq show TASK --full`. Defer `up --no-claude` if any matched run has `actual_provider: claude` and is in one of these states:

- `running`, `validating` or `needs_session`
- `awaiting_integration` with a non-null `lease` in `status`

If the service fails to start or answer, or you cannot read `status` or `show TASK --full` or match a relevant run, tell the person what failed, defer the switch and investigate using [the doctor reference](doctor.md). If Claude runs remain or their absence cannot be confirmed, bring back the supervisor with `up` without `--no-claude` on the person's word. Resolve the blockers and let any remaining Claude runs finish. Repeat the drain and check before switching.

**If you switch with runs still present.** The new supervisor considers adoption once the old lease is stale. This happens when the old supervisor's PID exits or the lease heartbeat expires, so after `down --wait` adoption can proceed without waiting for the heartbeat to expire. A run that cannot be adopted as a wait is deferred until a worker slot is free.

- **Normal adoption.** A `running` or `validating` run with a stale lease can be adopted if its wrapper reported exit, or if its wrapper has a live process and a fresh heartbeat. A live wrapper with an expired heartbeat fails that check. If the last resume event was `resume_skipped`, the run is eligible even with a missing or stale wrapper and can proceed through validation and review without a session. After an answer or a release, a next turn can go to an adopted wrapper whose process still lives and which has yet to report exit.

  A leased `awaiting_integration` run is eligible for normal adoption at any review stage, regardless of wrapper state. This includes a recorded revise wait preserved by the drain with its status, wait record and lease intact. The adopter continues from the review history. With a live session, a recorded request still to be sent, such as a revise or a conflict resolution, can become the existing wrapper's next turn. In `status`, a leased entry in `runs` has a `lease` object. A run that gave up its lease to wait for an `approve_landing` answer has `lease: null` and is outside adoption and the drain's preserved waits.

- **Recovery of an unadoptable run.** Once the old supervisor's PID has exited, a `running` run that cannot be adopted is eligible for recovery once the recovery checks pass. A recent lease heartbeat or a wrapper whose process still lives can keep it `running`. It becomes `interrupted` when the recovery write succeeds. A recovery round can be considered in that same supervisor pass if a slot is free and the other admission checks pass.

  `triaging (runtime)` can appear as soon as the interruption is recorded, before a round starts. A task hold keeps the run out of recovery job candidates and can leave that display in place until release. If an attempted launch results in `triage_failed` with `provider_unusable`, the runtime keeps `triaging (runtime)` and waits for the next round. While you see `triaging (runtime)`, leave triage to the runtime. The display can cover several stages of recovery.

  When the recovery route is unavailable to start, the runtime records `triage_failed` for manual handling. An unclosed authentication or usage-limit `queue_hold` ask targeting that recovery job suppresses `triage by hand`. Follow the ask while it applies. When `triage by hand` actually appears, follow [the manual triage steps](triage-by-hand.md#triage-by-hand).

- **Claude resumes.** The supervisor excludes Claude `needs_session` runs from resume candidates. It skips starting another resume, handing off a run whose attempts are exhausted, and taking the `resume_skipped` route that sends already resolved work to validation and toward landing. A resume already started under the old supervisor can still be adopted with a live wrapper that has yet to report exit. Its latest resume event is `resume_started`, followed by a `workspace_created` with a workspace ID for that attempt.

  Check the per-run entries in `status`'s `attention` after switching. Claude runs that the runtime classifies as resuming show `recover by hand` without `reason_category`. This includes held `needs_session` runs and runs that would show `waiting for a slot behind earlier work (runtime)` with Claude enabled. The attention covers runs excluded from resume candidates, which stay parked, and already-started resumes that the new supervisor adopts and can continue.

  To restore the usual resume decisions for parked runs, use `down --wait` and `up` without `--no-claude` on the person's word. The runtime still checks task holds, replanning fences, existing wrappers and attempts remaining. Leave opening resume workspaces to the supervisor. Do not use `recover`, `ready` or `cancel` in response to the Claude-resume attention.

  When the same `next` has `reason_category: recovery_failed`, it is a live-session failure recorded by an older runtime. Use the [live-session procedure](triage-by-hand.md#recover-by-hand) for that attention.

`status` shows one queue-wide `provider_disabled` attention while the supervisor runs under `--no-claude`. Its `handle disabled roles manually` next points to this skill's manual procedures for disabled roles. For individual Claude worker runs, use the switch and run-level checks above, including the `recover by hand` attention that names the run.

While a supervisor runs under `--no-claude`, `dagq inbox` opens only a usable Codex inbox or reports why it cannot. Keep the inbox already open. The flag implies no approval or bypass. Do not run `observe` in their place.

### A supervisor of another build

Replace the binary with `install` (`reference/update.md`), never `cp`. A supervisor of another build is handed over in place: runs go on. One that is drained instead: tell the person what it waits for and never kill it.

A command that stops with "run `dagq migrate`": `dagq migrate --check` lists the migrations; `up` and `install` apply the compatible ones, a breaking one needs `install --allow-breaking` (`reference/update.md`); tell the person first. `unsupported queue schema version`: use the newer binary.

A supervisor alive but no longer heartbeating is not replaced (`status` shows it stale): with no other live supervisor, the person stops it with `down --force`; beside a live one, see "Two supervisors of one queue". A registration whose supervisor died needs nothing.

### Runs waiting for a person

A stop drain (`down` or `down --wait`) finishes work in progress and lets waits that can proceed return to a slot. It preserves a slot run's current status, wait record and lease across the stop only in either of these cases:

- Its recorded phase is waiting for an answer, and a task hold or that run's unanswered ask blocks it.
- Its recorded phase is waiting to return, and a task hold blocks it.

This rule applies regardless of run status, including session waits in `running`, revise waits in leased `awaiting_integration` and waits during an already started resume in `needs_session`. The next supervisor applies its normal adoption checks to the preserved leases. A run that gave up its lease to await landing approval is outside the preserved waits. For the queue-use check after the drain in `install --allow-breaking`, see [Breaking migrations](update.md#breaking-migrations).

### A supervisor of the retired mode

One an earlier binary started outside launchd (`status` names its mode) keeps working, but nothing restarts it. Move it to launchd once: `down --wait`, then `up`, keeping your usual flags (for example `--auto-update`). The person closes the terminal it ran in.

### Two supervisors of one queue

When `status` or `doctor` reports duplicate supervisors, `up` alone reuses one and does not fix it. On the person's word, `down --wait`, then `up`. If an agent is still loaded, run the `launchctl bootout` the output names and remove its plist. A handoff's two rows under one pid are not a duplicate.

A stale registration beside a live supervisor is not a restart: `up` would only reuse the live one. A dead one needs nothing. A hung one shows `stop the stale supervisor's process`: on the person's word, check the pid is still a `dagq supervise` of this queue (`ps -p PID -o command`; if the pid was reused by another program, run `up`), then stop it (`kill PID`; not `down`, which stops the live one).

### Open the inbox again (the guardrail)

When `status` or `doctor` says the inbox's guardrail is off (`inbox_guardrail.guardrail` `false`, `reason` `opened_without_guardrail`), what is missing depends on its `provider`: a Claude inbox was opened without its settings, and a `claude` typed again in the inbox's terminal has none either and is not seen; a Codex inbox has no record that its PreToolUse guardrail hook was given at the start. A Codex inbox given the hook (`guardrail_by` `pre_tool_use`) that never ran (`reached` still `false`), or one whose start left a dagq hook untrusted, is reopened the same way (`skills/dagq-inbox/reference/claude-and-codex.md`); never by resuming the Codex session by hand. Reopen it the same way after a plugin update, or once to move an inbox an earlier binary opened in a cmux workspace. On the person's word:

1. The inbox writes its handoff first (open items, the watch's cursor, what the person said to keep), where the repository's instructions keep it.
2. The person ends that inbox (it cannot end itself).
3. The person closes that inbox's terminal or workspace if they no longer need it, and runs `dagq inbox` in a terminal without `DAGQ_ROLE` (it is refused inside an inbox): the new inbox has the settings, reads the handoff and starts its watch. Its flags: `dagq inbox --help`.

## down

- Plain `down` requests a stop. The supervisor stops claiming, finishes the runs that can proceed and exits, leaving the blocked waits above for the next supervisor.
- `--wait` blocks until it is gone. Use it when the next step needs that (changing flags, moving a supervisor of the retired mode, shutting down).
- `--force` loses the active runs (handled afterwards with this skill): the person types `dagq down --force` in a separate terminal without `DAGQ_ROLE` and confirms the target on its TTY. The inbox cannot relay this.
- Once no supervisor is left, `down` stops the queue's service too (at once with `--wait`, `--force` or no supervisor running; without `--wait`, when the last supervisor ends its drain). From then on `status` and `watch` fail as `unreachable`, with no outage record and no attention: that is the expected state, not a failure. The inbox tells the person once, does not restart its watch in a loop, and starts it again from the same cursor after `up`. `down` reaches the queue only through the service too: to read, `down` again or `recover` without a supervisor, start the service alone (`reference/doctor.md`).

## Logs

`"$DAGQ" locate` prints the log directory. Read them when a supervisor is missing from `status`, `up` reports that none registered, or a run failed for a reason `show` does not explain. Deleting old files is the person's call.
