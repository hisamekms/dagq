---
name: dagq-recover
description: Manual dagq recovery on the person's word, from the inbox or their terminal, after runtime recovery fails. Recover a run no supervisor serves; decide on a run whose recovery job failed; review and integrate a run whose headless review failed, or retry a failed push; answer a stalled ask, read a run's turns or a past stuck_exit or answer_prompt ask; bypass or resubmit a plan review; start, stop or update the runtime. Use when status or watch shows "recover run", "exit the session", "triage by hand", "recover by hand", "review by hand", "plan review by hand", "goal review by hand", "review and integrate", "push main", "restart supervisor", "send the answer of ask <id> to the worker", "plan task <N> with the answer", an answered stalled ask, or when the person asks to start, stop, update or recover. Entries ending in "(runtime)" need nothing.
---

# dagq: what a person does by hand

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` as in the `dagq` skill; the reference files below sit in `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/`. Never touch the queue database; never work around refusals. Ordinary recovery may be relayed by inbox; weakening operations and protected-path answers require the person's own TTY without `DAGQ_ROLE`, confirming the target. Never unset your role to act as the person (`skills/dagq/reference/authority.md`). Write for the person in the language the `dagq` skill's section 5 names.

Three layers: the runtime fixes what fixed rules prove safe (`auto_repaired`), the headless recovery job fixes what it can of the rest, and only what neither could, or a person's call (scope, discarding work, login, cost), reaches the person. Every `failed` / `interrupted` run, one whose resumes ran out (`resume_exhausted`) and a live run's alert go to the recovery job (`triaging (runtime)`); its `decide` or `stalled` ask opens only when it escalates, is not sure or used its three tries (a turn that ended without a receipt is nudged, then asked `stalled`; `skills/dagq-inbox/reference/status.md`). The next supervisor pass recovers a run without a lease whose processes are gone, and a `needs_session` run is resumed (`resuming (runtime)`). Do not recover, `ready` or close these runs. `status` beats `watch`. Under `--no-claude`, [Claude resumes](reference/up-down.md#explicitly-run-without-claude) can show `recover by hand`.

## 1. Diagnose without changing state

```sh
"$DAGQ" doctor --full
```

`reference/doctor.md`: its fields (`blockers`, `recoverable`) and common cases. A `running` or `validating` run whose wrapper is alive under a stale lease is adopted by the next supervisor: start one (`up`, section 5) instead of recovering it.

Disk claim hold or active relaxation: `reference/disk.md`.

## 2. Stop what is still running

Recovery is refused while a run process lives or its lease is fresh. A supervisor asks a run's session to exit and stops its background wrapper itself; nothing types into a session (`run send` refuses every run). `exit the session` (the runtime could not ask it to exit) and a hung supervisor are ended by the person in their own terminal; never kill them yourself.

To make a task wait while what blocks its run is removed, then go on: `hold` / `release` (`reference/hold.md`).

## 3. Recover (only without a supervisor)

Attention `recover run` (`kind` `runtime_error`): an unfinished run without a lease whose session may still live. With a supervisor running it recovers the run once the session is gone; without one:

```sh
"$DAGQ" recover RUN_ID
```

What it leaves: `reference/doctor.md`; the next supervisor's recovery job takes the run (`up`).

## 4. Triage by hand, and recover by hand

Follow `reference/triage-by-hand.md` when `triage by hand` appears. A `triage_failed` with `provider_unusable` waits for another round. For `recover by hand`, check the attention's `reason_category`: `recovery_failed` routes to the older live-session failure procedure in that reference. If the field is absent under `--no-claude`, follow [Explicitly run without Claude](reference/up-down.md#explicitly-run-without-claude). The triage reference also covers broken verification and carrying committed work over with `ready ID --inherit`.

## 5. Start, stop and update the runtime

```sh
"$DAGQ" up
"$DAGQ" down
"$DAGQ" down --wait
"$DAGQ" install --from PATH
```

For `restart supervisor`, run `up`. See `reference/up-down.md` for flags, duplicate or retired supervisors, and `inbox_guardrail` false.

`down` and `down --wait` finish work that can proceed. They preserve a slot run's status, wait record and lease across a stop only for a recorded answer wait blocked by a task hold or that run's unanswered ask, or a recorded return wait blocked by a task hold. See [Runs waiting for a person](reference/up-down.md#runs-waiting-for-a-person) for the phases and handover.

After draining, `install --allow-breaking` checks queue use. The `approve_update` ask's `install` answer uses that command. Any `claimed`, `starting`, `running`, `validating` or `integrating` run, regardless of lease, or any live wrapper or supervisor prevents migration. A lease-free `awaiting_integration` run pending landing approval is outside the handover and does not block migration by itself, even with an open ask.

On refusal, install reuses an answering old service or tries to start it with the old binary. If the service is available and there is a drained supervisor registration, it tries to restart that supervisor with the same arguments. A failed service recovery skips the restart. The error gives the original reason and both recovery results.

Before installing, show the person the asks and holds blocking these waits. Answer in their words or release holds as they direct. Check `status` for those runs to leave the in-use statuses before retrying. See `reference/update.md` for the steps and recovery.

## 6. Review by hand, and a failed push

Attention `review by hand` (`review_failed`: the supervisor's headless review failed) or `review and integrate` (a run accepted without a review): review it in a subagent from the file `review ID` writes, and on the person's word `integrate` it; on doubt, register an `approve_landing` ask. `push main` (`push_failed`): read `branch` and `remote` from the `repository` field of `"$DAGQ" doctor`, fix the cause, then `git push <remote> <branch>`. Follow `reference/review-by-hand.md`.

## 7. A run's session: turns, stalls, undelivered answers

Every run's session is headless in the background: nothing is typed by hand (`run screen`, `run send` refuse every run). Read what its turns did with `"$DAGQ" run log RUN [--follow]` (a planner's with `planner log ID`). The supervisor applies a `stalled` answer itself (`reference/stalled.md`); `send the answer of ask <id> to the worker and close it` and a past `stuck_exit` or `answer_prompt` ask: `reference/session.md` and `reference/stuck-exit.md`. Never `recover` such a run while a supervisor runs. A `needs_session` run the runtime resumes: `reference/resume.md`; never open a resume workspace yourself. Held by a replanning: `reference/replan-held.md`.

`plan task <N> with the answer: …` has no worker to reach: a planning request carries the person's answer (`reference/session.md`, "An undelivered planner answer about a draft").

## 8. Bypass plan review; a failed plan or goal review

Only plan review makes a task `ready`. A person may skip it with `"$DAGQ" ready ID --bypass-review` (a draft or submitted task; `review_bypassed`): typed by them in their own TTY without `DAGQ_ROLE`, per task, for an urgent fix, a failed plan review, or a concern they already decided. Never bypass as a habit or on a planner's judgment; a retry needs none (section 4).

`plan review by hand` (`plan_review_failed`) holds the submitted proposal.
`check the planner` covers `planner_unresponsive` and `planner_switch_stalled`.
A stalled switch returns work to normal planning. Carried answers went undelivered.
See `reference/plan-review-by-hand.md` for logs and options.

Attention `goal review by hand` (`goal_review_failed`): a goal review job failed; `reference/goal-review-by-hand.md`.
