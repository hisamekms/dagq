---
name: dagq-inbox
description: Be a dagq queue's inbox: start from status --role inbox, wait for its attention with watch --role inbox in the background, show each open ask (question, options, why a person is needed) to the person and write their answer back with answer, report every other attention (an answered ask, e.g. a stalled session's intervene, a stopped supervisor, a failed review, recovery job or plan review, an unresponsive planner, a failed push) to the person, and carry out only what the person says, through dagq-recover. Never decides by itself; what the runtime and the recovery job fix never reaches it. Use when the session starts or wakes up as a dagq inbox (DAGQ_ROLE=inbox), or when the person asks what the queue is waiting on them for. Registering work is dagq-planner.
---

# dagq: relay the queue's asks and attention to the person

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill (`"$DAGQ" --resolve`). Never open or edit the queue database; go through the CLI only.

Roles (ADR-0044): the **supervisor** claims, runs, validates, reviews, resumes and lands runs, and runs headless jobs (review, plan review, the **recovery job**); a **worker** is one run's session; a **planner** is an on-demand session that writes goals and tasks (`dagq-planner`); the **observer** is a periodic job. This session, the **inbox**, is the one resident session where everything that waits for the person reaches them: an **ask** and every other **attention**. Each ask also notifies it (`cmux notify`).

Only what needs a person comes here (ADR-0047). The runtime fixes known cases itself (an unsent Enter, an undelivered `/exit`, a known dialog, a resume or a stale receipt; `auto_repaired`), and the recovery job fixes what it can of the rest (failed runs, a stuck `/exit`, an unknown dialog, stuck background work). An ask opens only when they could not, and every ask says why a person is needed (`reason_category`: `scope`, `discard`, `authentication`, `cost`, `recovery_failed`). Do none of their work by hand.

This session holds no state of its own. After a restart, compaction or `/clear`, start again from step 1 (the plugin's SessionStart hook prints `status --role inbox`).

## 1. Read what waits

```sh
"$DAGQ" status --role inbox
```

`asks` lists the open asks; `attention` has everything that waits, each with a fixed `next`; `cursor` is where the next `watch` starts. Handle open asks first (step 3), then the rest (step 4). `${CLAUDE_PLUGIN_ROOT}/skills/dagq-inbox/reference/status.md` lists every field and `next`.

## 2. Watch in the background

Run `"$DAGQ" watch --role inbox --after <cursor>` in one loop of `reference/watch.md` under `run_in_background`. Never report an empty timeout (default `--timeout 600`: no `events`, cursor unchanged): a "nothing new" every ten minutes is noise. The loop watches again and ends only with `events` or `supervisors_changed`; handle them (steps 3, 4), then loop from its `cursor`. Keep exactly one watch running; never poll `status` in a loop.

## 3. Show an ask and write the answer

```sh
"$DAGQ" asks --open --role inbox
```

It prints each open ask in full, oldest first. Take them one at a time:

1. Show the person the question as written, its `reason_category`, `asked_by`, the task and run, and the options. Use `AskUserQuestion` with the options as choices when available. Add no recommendation of your own.
2. Write the answer exactly as the person gave it: the option's text, or their own words.

```sh
"$DAGQ" answer <id> --text '<the answer>'
```

3. `{"error": "ask <id> is not open"}` means the runtime closed it first (the dialog went, the session moved on or exited): tell the person and move on.

A run waiting on an ask holds no `--parallel` slot (`reference/status.md`, "Runs waiting for a person"). For context, read `"$DAGQ" show <task_id>` (`--full` for a receipt). Leave an ask the person does not want to answer yet open.

The kinds: `approve_landing` (a review's doubt: `land` / `send_back` / `cancel`), `approve_plan` (plan review's concern: `ready` / `send_back: <reason>` / `cancel`), `decide` (a failed, interrupted or resume-exhausted run the recovery job could not fix: `retry` / `resume` / `cancel`, no `resume` once resumes are used up, plus the job's options, which send the run back to the job), `stalled` (a session idle without a receipt after one nudge, `reason: idle_without_receipt`, a typed text the session did not take, `reason: send_unconfirmed`, or a job's escalation; the question lists background work and screen: `wait` (the supervisor closes it, counts again), `intervene` (a person steps in), `propose`; it closes itself once the session moves on; `reference/status.md`), `worker_question` (typed into the worker's terminal), `planner_question` (`adopt` / `cancel` / `keep_draft`), `answer_prompt` (a dialog the runtime and the job could not answer, or a resumed session's input box not ready, `input_not_ready`; no options: the answer is what to send, e.g. `enter`; the question ends with its screen), `stuck_exit` (a `/exit` neither could see through: `exit` / `wait`), `blocked` (the observer's, about one finding: `propose` (make it a proposal) / `dismiss`), `queue_hold` (login or cost, one per queue), and `update_failed` / `approve_update` (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/update.md`). The runtime applies `propose` (a planner of its own takes the finding) and `dismiss`. An answer the runtime does not apply comes back as `read the answer of ask <id> and close it` (step 4).

## 4. Report the other attention, act only on the person's word

Report each to the person in one short list (task, status, `next`, gist of `last_error`), and do what they say with the `dagq-recover` skill. `(runtime)` entries need nothing.

- `read the answer of ask <id> and close it` (`ask_answered`): an answer the runtime does not apply. `stuck_exit` `exit`, `answer_prompt`, `stalled` `intervene`, or the person's own text: carry it out as `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/session.md` says, then `"$DAGQ" ask close <id>`. `wait`, or nothing to do: `ask close <id>`.
- `send the answer of ask <id> to the worker and close it`: the supervisor could not type it; `session.md` too.
- `triage by hand` (`triage_failed`): an ended run's recovery job failed; `recover by hand` (`recovery_failed`): a live run's recovery job failed and its session waits. Show `last_error`; the person decides with `dagq-recover` section 4.
- `decide the draft in a planner` (`draft_planner_exhausted`), `decide the finding in a planner` (`finding_planner_exhausted`), `check the planner` (`planner_unresponsive`), `plan review by hand` (`plan_review_failed`): tell the person, who works in a planner (`dagq-recover` section 8).
- `install tool` (`run_env_program_missing`): a `[run.env]` program is not on the supervisor's PATH, so it claims and lands nothing; the person installs it. It clears by itself.
- `report the update` (`update_installed`): tell the person its `version` and `commit`.
- `fix the push command` (`kpi_push_abandoned`): the host's KPI push command (`[push]` of `host.toml`) failed one message three times and it was given up; tell the person, who fixes the command or its service. It clears with the next push that succeeds.
- `restart supervisor` (`supervisor_stopped`, `supervisor_stale`): `up` once the person says so (`dagq-recover`, section 5).
- `review by hand`, `review and integrate`, `push main`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`, with the person.
- `recover run`: the `dagq-recover` skill.

## Where your authority ends

Yourself: `status`, `watch`, `asks`, `show`, `answer` with the person's own words, and `ask close` after an answer was carried out. Only when the person says so: what `dagq-recover` describes (`up` / `down` / `install`, `integrate` after a review by hand, `recover`, a retry `ready`, `ready --bypass-review`, `cancel`, keys and `/exit` in a run's workspace). Never answer on the person's behalf, never pick a default, and never `add`, `goal add` or `goal close`: registering work is the planner's.
