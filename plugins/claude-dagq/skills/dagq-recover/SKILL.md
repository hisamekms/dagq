---
name: dagq-recover
description: What a person does by hand in a dagq queue, from the inbox or their own terminal, only on the person's word, once the runtime and the recovery job could not fix it. Recover a run no supervisor serves; decide on a run whose recovery job failed; review and integrate a run whose headless review failed, or push main; carry out a stuck_exit, answer_prompt or stalled (intervene) answer with run screen / run send; bypass or resubmit a plan review; start, stop or update the runtime. Use when status or watch shows "recover run", "exit the session", "triage by hand", "recover by hand", "review by hand", "plan review by hand", "goal review by hand", "review and integrate", "push main", "restart supervisor", "send the answer of ask <id> to the worker", an answered stuck_exit, answer_prompt or stalled ask, or when the person asks to start, stop, update or recover. Entries ending in "(runtime)" need nothing.
---

# dagq: what a person does by hand

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` as in the `dagq` skill; the reference files below sit in `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/`. Never touch the queue database; never work around refusals. All of it is done on the person's word (`up` / `down` / `install`, `integrate`, `review`, `recover`, `ready`, `cancel`, `answer`, `ask close`, `run send`) from the inbox, which the CLI records as delegated (actor `inbox`), or by the person in a terminal without `DAGQ_ROLE` (actor `user`). Not a planner's (`skills/dagq/reference/authority.md`). Write for the person in the language your prompt or the status's `language.instruction` names (`dagq` skill, section 5).

Three layers: the runtime fixes what fixed rules prove safe (`auto_repaired`), the headless recovery job fixes what it can of the rest, and only what neither could, or a person's call (scope, discarding work, login, cost), reaches the person. The next supervisor pass recovers a run without a lease whose processes are gone. Every `failed` / `interrupted` run, and one whose resumes ran out (`resume_exhausted`), goes to the recovery job (`triaging (runtime)`): it retries (carrying the commits over or not), resumes or waits; a `decide` ask opens only when it escalates, is not sure or used its three tries. A live run's alert (`long_background`, `idle_process`, `stuck_exit`, `prompt_waiting`) goes to the job too; its `stalled`, `stuck_exit` or `answer_prompt` ask opens only when the job cannot fix it (an idle session without a receipt is nudged once, then asked `stalled`). Also, a `needs_session` run is resumed (`resuming (runtime)`). Do not recover, `ready`, type into or close these runs. `status` beats `watch`.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

It shows the supervisors, each unfinished run's lease, processes and `blockers` (`recoverable: true` when empty); `reference/doctor.md`: fields and common cases. A `running` or `validating` run whose wrapper is alive under a stale lease is adopted by the next supervisor: start one (`up`, section 5) instead of recovering it.

## 2. Stop what is still running

Recovery is refused while a run process lives or its lease is fresh. The person ends them (`"$DAGQ" run send RUN --key exit`, also for `exit the session`, or stop a hung supervisor); never kill them.

## 3. Recover (only without a supervisor)

Attention `recover run` (`kind` `runtime_error`): an unfinished run without a lease whose session may still live. With a supervisor running it recovers the run once the session is gone; without one:

```sh
"$DAGQ" recover RUN_ID
```

The run becomes `interrupted` (an `integrating` one `awaiting_integration`); its worktree, workspace and run dir stay. The next supervisor's recovery job takes it (`up`).

## 4. Triage by hand, and recover by hand

Attention `triage by hand` (`triage_failed`): an ended run's recovery job failed; the run stays as it is and is not tried again. A live run's failed recovery job opens that alert's own ask (`stuck_exit`, `answer_prompt`, `stalled`; section 7); `recover by hand` (`recovery_failed`) is one an older runtime recorded, with the session untouched. Before bringing either to the person, read `reference/triage-by-hand.md`: what to read and do; it also keeps a broken verify's work (`edit`, then `retry_inherit`).

## 5. Start, stop and update the runtime

```sh
"$DAGQ" up            # add --parallel N, --max-waiting N, --auto-update, --claude EXE --codex EXE
"$DAGQ" up --in-cmux  # only when the preflight sends you there
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
"$DAGQ" install --from PATH  # swap the binary, hand over
```

`--plugin-dir`: only a path the repository names, never `$CLAUDE_PLUGIN_ROOT`. `up --no-claude` forbids Claude (Codex workers; unsupported roles by hand); drain before changing it.

`up` is idempotent: one resident supervisor and the inbox workspace; no planner (`plan` is refused; a plan is the inbox's request: `skills/dagq-inbox/reference/requests.md`). `inbox_guardrail` false (`status`, `doctor`): `reference/up-down.md`, "Open the inbox again". `restart supervisor` is answered with `up` (nothing else restarts an in-cmux supervisor). Update the binary with `install` (or `--rollback`), never `cp`; only a breaking migration drains (`--allow-breaking`). `up --auto-update` does it per runtime landing; it and the `update_failed` / `approve_update` asks: `reference/update.md`. A drain waits for runs waiting on an ask too. `down --force` loses the active runs: only on the person's explicit word. `--claude` and `--codex` (Codex workers) are fixed on the supervisor as real paths: from a cmux terminal pass `~/.local/bin/claude` and `~/.local/bin/codex`, and after updating either, `down --wait` and `up` again. `reference/up-down.md`: outcomes, the in-cmux case, logs.

## 6. Review by hand, and a failed push

Attention `review by hand` (`review_failed`: the supervisor's headless review failed) or `review and integrate` (a run accepted without a review): review it in a subagent from the file `review ID` writes, and on the person's word `integrate` it; on doubt, register an `approve_landing` ask. `push main` (`push_failed`): fix the cause, then `git push origin main`. Follow `reference/review-by-hand.md`.

## 7. A run's session: dialogs, stalls, stuck exits, undelivered answers

A `stuck_exit`, `answer_prompt` or `stalled` ask means the runtime (known dialogs, `/exit` retries, a nudge) or, for a live alert, the recovery job could not fix it. Its answer, and `send the answer of ask <id> to the worker and close it`, are carried out with `"$DAGQ" run screen RUN` and `run send RUN --key K` / `--answer ASK` (never `cmux`), never by `recover` while a supervisor runs. Follow `reference/session.md` (also `input_not_ready` and a `stalled` `send_unconfirmed`; `reference/stuck-exit.md` for `stuck_exit`; `reference/stalled.md` for a `stalled` `intervene`: the screen, background work, an instruction or `/exit`, then `ask close`). A headless run (`worker_mode: headless`) has no screen or keys: only its `stalled` ask applies (`reference/stalled.md`). What the runtime's resume sends and when it ends: `reference/resume.md`; never open a resume workspace yourself.

## 8. Bypass plan review; a failed plan or goal review

Only plan review makes a task `ready`. A person may skip it with `"$DAGQ" ready ID --bypass-review` (a draft or submitted task; `review_bypassed`): on their explicit word only, per task, for an urgent fix, a failed plan review, or a concern they already decided. Never bypass as a habit or on a planner's judgment; a retry needs none (section 4).

Attention `plan review by hand` (`plan_review_failed`): a proposal's headless plan review failed; it stays `submitted` and held. `check the planner` (`planner_unresponsive`): the person looks at that planner (`planner screen ID`). What to read and the person's choices: `reference/plan-review-by-hand.md`.

Attention `goal review by hand` (`goal_review_failed`): a goal review job failed; `reference/goal-review-by-hand.md`.
