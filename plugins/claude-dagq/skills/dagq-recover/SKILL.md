---
name: dagq-recover
description: What a person does by hand in a dagq queue, from the inbox or a planner session and only on the person's word, once the runtime and the recovery job could not fix it. Recover a run no supervisor serves; decide on a run whose recovery job failed; review and integrate a run whose headless review failed, or push main; carry out a stuck_exit or answer_prompt answer in a run's cmux workspace; bypass or resubmit a plan review; start, stop or update the runtime (up / down / install). Use when status or watch shows "recover run", "triage by hand", "recover by hand", "review by hand", "plan review by hand", "review and integrate", "push main", "restart supervisor", "send the answer of ask <id> to the worker", an answered stuck_exit or answer_prompt ask, or when the person asks to start, stop, update or recover. Entries ending in "(runtime)" need nothing.
---

# dagq: what a person does by hand

Prerequisite: resolve the launcher as in the `dagq` skill (`DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`). Never touch the queue database; the binary refuses unsafe recoveries, so do not work around it. Everything here is done from the inbox or a planner session, when the person says so (ADR-0044 decision 6).

Three layers (ADR-0047): the runtime fixes what fixed rules prove safe (`auto_repaired`), the headless recovery job fixes what it can of the rest, and only what neither could, or a person's call (scope, discarding work, login, cost), reaches the person. The next supervisor pass recovers a run without a lease whose processes are gone. Every `failed` / `interrupted` run, and one whose resumes ran out (`resume_exhausted`), goes to the recovery job (`triaging (runtime)`): it retries, retries carrying the commits over, resumes or waits; a `decide` ask opens only when it escalates, is not sure or used its three tries. A live run's alert (`long_background`, `idle_process`, `stuck_exit`, `prompt_waiting`) goes to the job too; its `stalled`, `stuck_exit` or `answer_prompt` ask opens only when the job cannot fix it. Also, a `needs_session` run is resumed (`resuming (runtime)`). Do not recover, `ready`, type into or close these runs. `status` beats `watch`.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

Explain what is still alive: the supervisors, each unfinished run's lease, processes and `blockers` (`recoverable: true` when empty). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/doctor.md` lists the fields and the common cases. A `running` or `validating` run whose wrapper is alive under a stale lease is adopted by the next supervisor: start one (`up`, section 5) instead of recovering it.

## 2. Stop what is still running

Recovery is refused while a process of the run lives or its lease is fresh. The person ends them (`/exit` in its workspace, or stopping a hung supervisor); never kill processes yourself.

## 3. Recover (only without a supervisor)

Attention `recover run` (`kind` `runtime_error`): an unfinished run without a lease whose session may still live. With a supervisor running it recovers the run once the session is gone; without one:

```sh
"$DAGQ" recover RUN_ID
```

The run becomes `interrupted` (an `integrating` one `awaiting_integration`), other runs are untouched, and the worktree, workspace and run directory are kept. The next supervisor's recovery job takes it; start one (`up`) rather than deciding yourself.

## 4. Triage by hand, and recover by hand

Attention `triage by hand` (`kind` `triage_failed`): the recovery job of a `failed` / `interrupted` (or `resume_exhausted`) run could not start, timed out, printed no verdict, or its verdict could not be applied; the run stays as it is and is not tried again. Read `last_error` and the run directory's `recovery-<alert>-<attempt>.prompt.txt`, `.out`, `.err`, and bring the choice to the person. On their answer: `"$DAGQ" ready ID` runs the task again as a new run (unchanged, so no plan review; a task to change goes to a planner, `dagq plan`), `cancel ID` drops it. Its workspaces stay open until then (the supervisor closes them after), so read the screen first. Close one by hand (`cmux workspace close <workspace_id>`) only while no supervisor runs.

Attention `recover by hand` (`kind` `recovery_failed`): a live run's recovery job failed (`last_error` its error); the session is untouched and the job does not run again for that alert. Read the screen with the person and carry out what they decide as in section 7. It clears once the session exits.

## 5. Start, stop and update the runtime

```sh
"$DAGQ" up --plugin-dir "$CLAUDE_PLUGIN_ROOT"            # add --parallel N, --max-waiting N, --auto-update
"$DAGQ" up --in-cmux --plugin-dir "$CLAUDE_PLUGIN_ROOT"  # only when the preflight sends you there
"$DAGQ" down            # stop claiming; the supervisor drains its runs and exits
"$DAGQ" down --wait     # the same, and block until it is gone
"$DAGQ" install         # build main, swap the binary, hand over
```

`up` is idempotent: one resident supervisor and the inbox workspace, no planner (a person opens each with `dagq plan`). `restart supervisor` is answered with `up` (nothing else restarts an in-cmux supervisor). Update the binary with `install` (or `--rollback`), never `cp`: supervisors take it over without waiting; only a breaking migration drains (`--allow-breaking`). `up --auto-update` does it per runtime landing; it and the `update_failed` / `approve_update` asks: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/update.md`. A drain waits for runs waiting on an ask too. `down --force` kills the supervisor and loses its active runs: only on the person's explicit word. `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/up-down.md` has the outcomes, the in-cmux case and the logs.

## 6. Review by hand, and a failed push

Attention `review by hand` (`review_failed`: the supervisor's headless review failed) or `review and integrate` (a run accepted without a review): review it in a subagent from the file `review ID` writes, and on the person's word `integrate` it; on doubt, register an `approve_landing` ask. `push main` (`push_failed`): fix the cause, then `git push origin main`. Follow `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`.

## 7. A run's session: dialogs, stuck exits, undelivered answers

A `stuck_exit` or `answer_prompt` ask means the runtime (known dialogs, `/exit` retries) and the recovery job could not fix it. Its answer, and `send the answer of ask <id> to the worker and close it`, are carried out in the run's cmux workspace with keys and text, never by recovering the run. Follow `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md` (and `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/stuck-exit.md` for `stuck_exit`). What the runtime's resume of a `needs_session` run sends and when it ends is in `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/resume.md`; never open a resume workspace yourself.

## 8. Bypass plan review, and a failed plan review

Only plan review makes a task `ready` (ADR-0044 decision 8). A person may skip it with `"$DAGQ" ready ID --bypass-review` (a draft or submitted task; `review_bypassed`): on their explicit word only, per task, for an urgent fix, a failed plan review, or a concern they already decided. Never bypass as a habit or on a planner's own judgment; a retry needs no bypass (section 4).

Attention `plan review by hand` (`kind` `plan_review_failed`): the headless plan review of a proposal could not start, timed out or printed no valid verdict; the proposal stays `submitted` and held, and is not reviewed again by itself. Read `last_error` and the job's files under `<queue dir>/plan-reviews/<plan review id>/` (the job's ID, not the proposal's), `"$DAGQ" proposal show ID` and the tasks, and bring the choice to the person: `ready --bypass-review` each task, have a planner (theirs, `dagq plan`) run `"$DAGQ" submit --proposal ID` to send it through plan review again as it is, or `cancel` the tasks. `check the planner` (`planner_unresponsive`): the person looks at that planner's workspace (`dagq planners`); nothing is closed or readied for it.
