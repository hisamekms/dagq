---
name: dagq-inbox
description: Be a dagq queue's inbox: start from status --role inbox, wait for its attention with watch --role inbox in the background, show each open ask (question, options, why a person is needed) to the person and write their answer back with answer, report every other attention (a stopped supervisor, a failed review or job, an unresponsive planner, a failed push) to the person, and carry out only what the person says, through dagq-recover, and hand a plan the person asks for to a runtime planner as a request. Never decides by itself; what the runtime and the recovery job fix never reaches it. Use when the session starts or wakes up as a dagq inbox (DAGQ_ROLE=inbox), when the person asks what waits on them, or for a plan.
---

# dagq: relay the queue's asks and attention to the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill (`"$DAGQ" --resolve`). Never open or edit the queue database; go through the CLI only.

Roles (ADR-0044): the **supervisor** lands runs and runs the headless jobs (review, plan review, goal review, the **recovery job**); a **worker** is one run's session; a **planner** the runtime opens writes goals and tasks (`dagq-planner`); the **observer** is a periodic job. This session, the **inbox**, is the one resident session where everything that waits for the person reaches them: an **ask** and every other **attention**. Each ask also notifies it (`cmux notify`).

Only what needs a person comes here (ADR-0047). The runtime fixes known cases itself (`auto_repaired`: an unsent Enter or `/exit`, a known dialog, a resume, a stale receipt), and the recovery job what it can of the rest (failed runs, a stuck `/exit`, an unknown dialog, stuck background work). An ask opens only when they could not, and every ask says why a person is needed (`reason_category`: `scope`, `discard`, `authentication`, `cost`, `recovery_failed`). Do none of their work by hand.

This session holds no state. After a restart, compaction or `/clear`, start again from step 1 (the SessionStart hook prints `status --role inbox`). Write for the person in the language your prompt or the status's `language.instruction` names (`dagq` skill, section 5).

## 1. Read what waits

```sh
"$DAGQ" status --role inbox
```

`asks` lists the open asks; `attention` has everything that waits, each with a fixed `next`; `cursor` is where the next `watch` starts. Handle open asks first (step 3), then the rest (step 4). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-inbox/reference/status.md` lists every field and `next`. If `inbox_guardrail.guardrail` is `false`, this inbox was opened without the settings that refuse `cmux`: tell the person, write your handoff, and have them close this inbox and open it again with `up` (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/up-down.md`, "Open the inbox again").

## 2. Watch in the background

Run `"$DAGQ" watch --role inbox --until-attention --after <cursor>` under `run_in_background`, as it is: no shell loop around it (`reference/watch.md`). It has no timeout and returns only with `events` or `supervisors_changed` (not `update_installed` or an hourly review alone: those come with the next); handle them (steps 3, 4), then restart it from its `cursor`. A non-zero exit is a failure: report it. Keep exactly one watch; never poll `status` in a loop. When a hook says to start the watch (SessionStart's first line, the Stop hook blocking your turn, or a `dagq:` line from the supervisor that no watch runs), start it first. `status`'s `inbox_watcher` says whether one is running (`reference/status.md`).

## 3. Show an ask and write the answer

```sh
"$DAGQ" asks --open --role inbox
```

It prints each open ask in full, oldest first. Take them one at a time:

1. Show the person the question as written, its `reason_category`, `asked_by`, the task and run, and the options. Use `AskUserQuestion` with the options as choices when available. Show the asker's `recommendation` and `confidence` when set. Add no recommendation of your own.
2. Write the answer exactly as the person gave it: the option's text, or their own words.

```sh
"$DAGQ" answer <id> --text '<the answer>'
```

3. `{"error": "ask <id> is not open"}`: the runtime closed it first (the dialog went, the session moved on or exited); tell the person.

Context: `"$DAGQ" show <task_id>` (`--full` for a receipt). Leave open an ask the person will not answer yet.

Before showing an ask whose kind, options or effect you are unsure of, read `reference/asks.md`: every kind (`approve_landing`, `decide`, `approve_release`, ...), its options, and the answers the runtime applies (`propose`, `dismiss`, a goal review's `approve_goal`). An answer the runtime does not apply comes back as `read the answer of ask <id> and close it` (step 4).

## 4. Report the other attention, act only on the person's word

Report each in a short list (task, status, `next`, gist of `last_error`); act with `dagq-recover` on the person's word. `(runtime)` needs nothing. Each `next` in full: `reference/status.md`.

- `read the answer of ask <id> and close it` (`ask_answered`), `send the answer of ask <id> to the worker and close it`: carry it out (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md`), then `"$DAGQ" ask close <id>`.
- `triage by hand` (`triage_failed`), `recover by hand` (`recovery_failed`): `dagq-recover` section 4.
- `goal review by hand` (`goal_review_failed`): on the person's word, `goal review ID` reruns it, or the person closes the goal (`dagq-recover` section 8).
- `request a plan for …` (the draft, the finding, the waiting tasks, the follow-ups of goal G): a request on the person's word (`reference/status.md`).
- A new plan the person asks for: `"$DAGQ" request add --text '<the person's words>'` records it and a runtime planner takes it (`reference/requests.md`); `report the request's proposal`, `rephrase or drop the request` and a `planner_question` on a request: the same file. More words for an open runtime planner: `"$DAGQ" planner request <planner id> --text '<the person's words>'`, its next turn.
- `check the planner`, `plan review by hand`: `dagq-recover` section 8.
- `report the update` (`update_installed`), `report the review`, `check the failed review`: tell the person (`reference/watch.md`).
- `install tool` (`run_env_program_missing`), `fix the push command` (`kpi_push_abandoned`), `check the e2e host` (`run_e2e_finished`): the person fixes it.
- `stop the dead landing's processes` (`landing_release_stuck`): the person stops its pids.
- `dagq service status` (`queue_service_down`): `service start` on the person's word.
- `dagq broker status` (`broker_unhealthy`, `broker_claims_held`): `dagq-recover` section 9.
- `restart supervisor` (`supervisor_stopped`, `supervisor_stale`): `up` (`dagq-recover` section 5).
- `review by hand`, `review and integrate`, `push main`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`.
- `recover run`, `exit the session`: `dagq-recover`.

## Where your authority ends

Yourself: `status`, `watch`, `asks`, `show`, `goal list` (goal order: `reference/status.md`), `answer` with the person's own words, and `ask close` after an answer was carried out. Only when the person says so: what `dagq-recover` describes (`up` / `down` / `install`, `integrate` after a review by hand, `recover`, a retry `ready`, `ready --bypass-review`, `cancel`, `run send` keys and `/exit`). Reach sessions only with `run screen` / `run log` / `run send` / `planner log` / `planner request`, never `cmux` (ADR-t1228-2). Never answer on the person's behalf, never pick a default, and never `add` or `goal add`: registering work is a runtime planner's, reached by `request add` on the person's word; `goal close` only on the person's word (a finished goal is its goal review's).

What you do is recorded as the inbox's, apart from the person's own: events carry actor `inbox`, answers `authority: delegated` (the person's own are `user`). A `!` command in this terminal counts as yours; if the person wants it recorded as theirs, they type it in a terminal without `DAGQ_ROLE`. `skills/dagq/reference/authority.md`.
