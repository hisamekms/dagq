---
name: dagq-recover
description: What a person does by hand in a dagq queue, from the inbox or their own terminal, only on the person's word, once the runtime and the recovery job could not fix it. Recover a run no supervisor serves; decide on a run whose recovery job failed; review and integrate a run whose headless review failed, or retry a failed push; carry out a stuck_exit, answer_prompt or stalled (intervene) answer with run screen / run send; bypass or resubmit a plan review; start, stop or update the runtime. Use when status or watch shows "recover run", "exit the session", "triage by hand", "recover by hand", "review by hand", "plan review by hand", "goal review by hand", "review and integrate", "push main", "restart supervisor", "dagq broker status", "send the answer of ask <id> to the worker", an answered stuck_exit, answer_prompt or stalled ask, or when the person asks to start, stop, update or recover. Entries ending in "(runtime)" need nothing.
---

# dagq: what a person does by hand

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` as in the `dagq` skill; the reference files below sit in `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/`. Never touch the queue database; never work around refusals. All of it is done on the person's word (`up` / `down` / `install`, `integrate`, `review`, `recover`, `ready`, `cancel`, `answer`, `ask close`, `run send`) from the inbox, which the CLI records as delegated (actor `inbox`), or by the person in a terminal without `DAGQ_ROLE` (actor `user`). Not a planner's (`skills/dagq/reference/authority.md`). Write for the person in the language the `dagq` skill's section 5 names.

Three layers: the runtime fixes what fixed rules prove safe (`auto_repaired`), the headless recovery job fixes what it can of the rest, and only what neither could, or a person's call (scope, discarding work, login, cost), reaches the person. Every `failed` / `interrupted` run, one whose resumes ran out (`resume_exhausted`) and a live run's alert go to the recovery job (`triaging (runtime)`); its `decide`, `stalled`, `stuck_exit` or `answer_prompt` ask opens only when it escalates, is not sure or used its three tries (an idle session without a receipt is nudged once, then asked `stalled`; `skills/dagq-inbox/reference/status.md`). The next supervisor pass recovers a run without a lease whose processes are gone, and a `needs_session` run is resumed (`resuming (runtime)`). Do not recover, `ready`, type into or close these runs. `status` beats `watch`.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

`reference/doctor.md`: its fields (`blockers`, `recoverable`) and common cases. A `running` or `validating` run whose wrapper is alive under a stale lease is adopted by the next supervisor: start one (`up`, section 5) instead of recovering it.

## 2. Stop what is still running

Recovery is refused while a run process lives or its lease is fresh. The person ends them (`"$DAGQ" run send RUN --key exit`, also for `exit the session`, or stop a hung supervisor); never kill them.

## 3. Recover (only without a supervisor)

Attention `recover run` (`kind` `runtime_error`): an unfinished run without a lease whose session may still live. With a supervisor running it recovers the run once the session is gone; without one:

```sh
"$DAGQ" recover RUN_ID
```

What it leaves: `reference/doctor.md`; the next supervisor's recovery job takes the run (`up`).

## 4. Triage by hand, and recover by hand

Attention `triage by hand` (`triage_failed`): an ended run's recovery job failed; the run stays as it is and is not tried again (a `triage_failed` with `provider_unusable` is not this attention: its next round starts on the other provider). `recover by hand` (`recovery_failed`): an older runtime's record. Before bringing either to the person, read `reference/triage-by-hand.md` (also a broken verify's work: `edit`, then `retry_inherit`; and carrying an ended run's committed work over by hand: `ready ID --inherit`).

## 5. Start, stop and update the runtime

```sh
"$DAGQ" up            # add --parallel N, --max-waiting N, --auto-update, --claude EXE --codex EXE
"$DAGQ" up --in-cmux  # only when the preflight sends you there
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
"$DAGQ" install --from PATH  # swap the binary, hand over
```

`--plugin-dir`: only a path the repository names, never `$CLAUDE_PLUGIN_ROOT`. `up --no-claude` forbids Claude (Codex workers; unsupported roles by hand); drain before changing it.

`up` is idempotent: one resident supervisor and the inbox workspace; no planner (`plan` is refused; a plan is the inbox's request: `skills/dagq-inbox/reference/requests.md`). `inbox_guardrail` false (`status`, `doctor`): `reference/up-down.md`, "Open the inbox again". `restart supervisor` is answered with `up`. Update the binary with `install` (or `--rollback`), never `cp`; only a breaking migration drains, and a drain waits for runs waiting on an ask too; `up --auto-update`, `--allow-breaking` and the `update_failed` / `approve_update` asks: `reference/update.md`. `down --force` loses the active runs: only on the person's explicit word. `--claude` / `--codex` take real paths (from a cmux terminal `~/.local/bin/claude`, `~/.local/bin/codex`). `reference/up-down.md`: outcomes, drains, the in-cmux case, logs.

## 6. Review by hand, and a failed push

Attention `review by hand` (`review_failed`: the supervisor's headless review failed) or `review and integrate` (a run accepted without a review): review it in a subagent from the file `review ID` writes, and on the person's word `integrate` it; on doubt, register an `approve_landing` ask. `push main` (`push_failed`): read `branch` and `remote` from the `repository` field of `"$DAGQ" doctor`, fix the cause, then `git push <remote> <branch>`. Follow `reference/review-by-hand.md`.

## 7. A run's session: dialogs, stalls, stuck exits, undelivered answers

The answer of a `stuck_exit`, `answer_prompt` or `stalled` ask, and `send the answer of ask <id> to the worker and close it`, are carried out with `"$DAGQ" run screen RUN` and `run send RUN --key K` / `--answer ASK` (never `cmux`), never by `recover` while a supervisor runs. Follow `reference/session.md` (also `input_not_ready`, `send_unconfirmed` and a headless run, which has no screen or keys; one in the background is read with `"$DAGQ" run log RUN [--follow]`, a planner's with `planner log ID`), `reference/stuck-exit.md` for `stuck_exit` and `reference/stalled.md` for a `stalled` `intervene`. What the runtime's resume sends and when it ends: `reference/resume.md`; never open a resume workspace yourself.

## 8. Bypass plan review; a failed plan or goal review

Only plan review makes a task `ready`. A person may skip it with `"$DAGQ" ready ID --bypass-review` (a draft or submitted task; `review_bypassed`): on their explicit word only, per task, for an urgent fix, a failed plan review, or a concern they already decided. Never bypass as a habit or on a planner's judgment; a retry needs none (section 4).

Attention `plan review by hand` (`plan_review_failed`): a proposal's headless plan review failed; it stays `submitted` and held. `check the planner` (`planner_unresponsive`): the person looks at that planner (`planner log ID`). What to read and the person's choices: `reference/plan-review-by-hand.md`.

Attention `goal review by hand` (`goal_review_failed`): a goal review job failed; `reference/goal-review-by-hand.md`.

## 9. The resource broker

Attention `dagq broker status` (`broker_unhealthy`, or `broker_claims_held` under mode `required`, which claims nothing meanwhile) clears by itself. Diagnose with `"$DAGQ" broker status`; what each code asks of the person (podman, dagq's Podman machine, `broker stop` / `broker start`), lowering a host to `disabled`, and a run failed on a broker error: `reference/broker.md`.
