# status, watch and run states

Read this when what `status`, `watch` or `show` says, or what to do about an attention, is unclear. Each command's flags and output: `"$DAGQ" <command> --help`.

## status

- `supervisors`: one entry per process that owns runs.
  - `registered`, `alive` and not `stale`: the resident supervisor is healthy, even with no runs.
  - `stale`: it died or hangs. On the person's word, `up` starts a fresh one; orphaned runs go through the `dagq-recover` skill, one run at a time. A stale registration whose process is still alive is not replaced: report it and let the person stop that process (`down --force` if it is the only one).
  - `registered: false`: a process holding a lease, normally an `integrate` landing a run.
  - Empty: nothing serves this queue; `up` on the person's word.
  - `slots` and `waiting` say how many runs hold a slot, wait to land and wait for a person; count them from these, never from the number of `runs`.
- `inbox_guardrail` (also in `doctor`): `false` means this inbox was opened without its settings. Tell the person and, after your handoff, have them reopen it with `dagq inbox` (`dagq-recover`'s `reference/up-down.md`, "Open the inbox again"). A `claude` typed again in the inbox's terminal has no settings whatever this says.
- `inbox_watcher` (also in `doctor`): `watching` `0` means no watch runs: start the one of `watch.md`. The Stop hook blocks your turn while it is `0`. When `absent_secs` was long, tell the person: asks opened meanwhile reached nobody.
- `sccache`: when the build cache server the run environment names needs a person, act on its attention below; `doctor` shows it (`${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/doctor.md`, "sccache server").
- `runs` (also in `doctor`): the unfinished runs and the runs a lease holds. Progress is the phase moving while the run holds its slot; when there is no phase, do not guess one. A person is needed only where `slot` is `waiting` (answer its ask) or `attention` names the run. A run's session is read with `run log` (below).
- `waiting`: the runs out of their slots while they wait for a person (below).
- `asks`: the open asks, cut short; read them whole with `asks --open --role inbox`.
- `ci`: the CI of the watched branch. A red CI needs nothing from the inbox: a runtime planner registers the fix. The failing tests: `"$DAGQ" ci failures`.
- `language`: write for people in it (the `dagq` skill's section 5).
- `attention`: act on each by its `next` (below); `--role inbox` keeps all of it.

## Each attention's next

A `(runtime)` next needs nothing: `reviewing`, `resuming`, `triaging`, `queued to land`, `applying the answer of ask <id>`, `delivering the answer of ask <id>`. When parked runs pile up and other work should go first, a person, or the inbox at a person's word, may `set-priority TASK low` on them (the `dagq` skill's `reference/inspect.md`, "Slot order").

- Asks:
  - `answer ask <id>`: an open ask; show it (the skill's step 3, `reference/asks.md`).
  - `read the answer of ask <id> and close it`: an answered ask the runtime does not apply. Carry out what the answer asks through `dagq` (the `dagq-recover` skill), never by typing into a session, then `"$DAGQ" ask close <id>`. A past `stuck_exit` or `answer_prompt`, `wait`, or nothing to do: just `ask close <id>`.
  - `send the answer of ask <id> to the worker and close it`: the supervisor could not deliver a worker's answer, or the session takes none any more: `dagq-recover`'s `reference/session.md`.
  - `plan task <N> with the answer: request add --ref task:<N> --ref ask:<id>, then close ask <id>`: a planner's answer about a draft no planner carries. Record the person's answer as a planning request in their words, naming the draft and the ask, then close the ask (`dagq-recover`'s `reference/session.md`). Never send it to a worker.
- Review and landing:
  - `review by hand`: a headless review failed with no open `approve_landing` ask; the person reviews it (the `dagq-recover` skill, section 6).
  - `review and integrate`: an accepted run nobody reviews (a supervisor gave it up mid-review: read what its session did first). The same section.
  - A review's concern or failure shows only as its `approve_landing` ask.
  - `push main`: a landed run whose push failed: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-recover/reference/review-by-hand.md`.
  - `check the e2e host`: the e2e a reviewed run needs could not run several times in a row. The person checks the host in their own terminal without `DAGQ_ROLE` (`dagq doctor`'s `run_env`, the host's e2e lock file). The runtime keeps retrying and the attention clears by itself once an e2e runs; nothing to close. If the run is left as `review and integrate`, check its e2e before integrating by hand.
  - `stop the dead landing's processes`: a dead landing left processes that hold the integration slot. Show the person the pids (`dagq show TASK --full`); the person stops those pids, never by name or pattern. The next supervisor pass releases the run.
- Recovery:
  - `triage by hand`: the recovery job of an ended run could not start or apply its verdict; the person decides (the `dagq-recover` skill, section 4).
  - `recover by hand`: a live run's failed recovery job that an older runtime recorded; the same section. A newer one opens a `stalled` ask instead.
  - `recover run`: a run its supervisor gave up; the supervisor recovers it once its processes are gone, and while they live, the `dagq-recover` skill. If `watch` reported it but `status` shows the run at rest, `status` wins.
  - `exit the session`: a run given up whose session's exit request could not be written. Tell the person, who ends it in their own terminal (the `dagq-recover` skill, section 2).
  - A turn that ended without a receipt is no attention: the runtime and its recovery job handle it. Do nothing about it until a `stalled` ask comes.
- Planning:
  - `plan review by hand`: the headless plan review failed and holds the proposal (the `dagq-recover` skill, section 8).
  - `check the planner`: a planner did not submit a revise in time, or a turn of a runtime planner hit its time limit; the person looks at it (`"$DAGQ" planner log ID`; section 8).
  - `goal review by hand`: a goal's review could not reach a verdict and is not tried again until its input changes. On the person's word, `goal review ID` reruns it, or the person closes the goal or asks for tasks through a planning request (section 8).
  - `request a plan for the draft`: no planner adopted, dropped or asked about a draft. Tell the person; on their word, record a planning request naming it (`request add --text '<their words>' --ref task:N`), or give it a revisit time (`"$DAGQ" revisit N --at <RFC 3339 time>`, `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/register.md`, "A draft's revisit time"), or the person decides it in their own terminal.
  - A draft a planner kept with `keep_draft` has no attention and waits for a request or its revisit time (`show ID`'s `revisit`); on the person's word, set, change or clear that time (`revisit ID --at <RFC 3339 time>` / `--clear`).
  - `request a plan for the follow-ups of goal G`: unfinished follow-ups of goal G have no settled membership, so its goal review waits. Tell the person; on their word, record a planning request naming the goal or the follow-ups (`--ref goal:G` or `--ref task:N`). The runtime never judges or moves them itself.
  - `request a plan for the finding`: no planner submitted, dismissed or asked about a finding. On the person's word, a planning request naming it (`--ref finding:N`), or the person submits or dismisses it in their own terminal.
  - `request a plan for the waiting tasks`: tasks of open goals wait on a task of a closed goal that will never complete. On the person's word, a planning request naming the stranded task (`--ref task:A`), or the person decides in their own terminal.
  - A request's outcomes and its planner's question: `reference/requests.md`.
- The host and its services (the person fixes them; tell them `last_error`):
  - `stop sccache on the host; the supervisor starts it`: the supervisor could not replace an unhealthy server. Show `doctor`'s server identity; the person stops it and leaves the start to the supervisor.
  - `install tool` (`run_env_program_missing`, `ci_watch_unavailable`): a program the run environment or the CI watch needs is missing on the supervisor's PATH, and claims or landings wait. The person installs it, or asks through a planning request for a task that stops naming it.
  - `log in to gh` (`ci_watch_unavailable`): the person runs `gh auth login` in their own terminal; the inbox does not log in.
  - `fix dagq.toml` (`ci_watch_unavailable`, `ci_jobs_missing`, `container_runtime_unavailable`): the CI watch's remote or job names, or the container runtime, do not work as configured. The person fixes it, or on their word the inbox asks through a planning request for a task that changes the configuration.
  - `fix the push command`: the host's push command dropped a message; the person fixes the command or its service. It clears only when a later push succeeds.
  - `dagq service status`: the queue service stopped. Show `dagq service status` and the service's log; at the person's word, `dagq service start` (or `up`).
  - `read the queue broker's log and run up`: show the broker's log; at the person's word, `up`.
  - `look at the dagq podman machine's relays`: show `podman machine list` and the error; the person mends the machine.
  - `restart supervisor`: `up` on the person's word. `supervisor_duplicate`: `down --wait`, then `up` (`dagq-recover`'s `reference/up-down.md`).
  - A worker provider that cannot be used is no attention: its runs move to the other provider, and the person acts only on a `queue_hold` ask (`reference/asks.md`).
- Notices (they do not stay in `status`; tell the person):
  - `report the update`: the supervisor updated dagq by itself, or only the plugin. Tell the person the version and what the notice says, and that sessions reopened after it load the updated plugin. Nothing to do; a failed update is an ask instead.
  - `report the review`, `check the failed review`: `watch.md`, "The throughput review's notice".
  - `check the failed observer`: `watch.md`, "The observer's failure notice".

## Runs waiting for a person (outside the slots)

A run whose session only waits for a person's answer leaves its slot while it waits, so another task can run; its ask is the attention, so answering the ask is all it needs from you. A drain (`down --wait`) waits for these runs too, so it ends only once their asks are answered. `status`' `waiting` lists them; their limit is `up --max-waiting` (`"$DAGQ" up --help`).

## asks

`"$DAGQ" asks` lists the asks nobody closed (`--open`, `--role inbox`, `--all`: `asks --help`). Show each ask's reason category with its question.

- `ask close <id>` marks an answered ask read. An unanswered ask cannot be closed: to withdraw one on the person's word, answer it first (`"$DAGQ" answer <id> --text "withdrawn: <why>"`), then close it.
- `ask` notifies nobody; your watch tells the person of new asks through the host's push, when it has one.

## watch and events

`watch` is the inbox's only wait (`watch.md`). `"$DAGQ" events --after <cursor>` returns the same attention events without waiting (`events --help`).

## Watching a background session

Every worker's and runtime planner's session runs as a background process with no screen; its wrapper writes each turn to its log. When the person wants to see what one does, or before you report one that seems stuck:

```sh
"$DAGQ" run log RUN                # the run id, or the task id for its latest run
"$DAGQ" run log RUN --lines 40     # the last 40 lines
"$DAGQ" planner log ID             # a runtime planner's, the id from `planners`
```

Never build the log's path yourself. `--follow` keeps printing: run it in the background, or let the person run it. `"$DAGQ" timeline RUN` gives the times. Nothing can be typed into a session: an answer goes as its next turn.

## show

`"$DAGQ" show ID` reads one task, with long texts cut; `--full` prints them whole (`show --help`). `"$DAGQ" list --status in_progress` finds the tasks worth a `show`. `stats` (`stats --help`, the `dagq` skill's `reference/inspect.md`) reads run events; leave proposals from it to the observer.

## goal list

When the person asks which goals go first, what moves now or what a theme holds, show them `"$DAGQ" goal list` (`--tag TAG` narrows it to a theme). Show the order as it prints, with each goal's title, priority and tags; never sort by ID or by your own reading. A goal's tasks and where each task's priority comes from: `"$DAGQ" goal show ID`. Changing a goal's priority or tags is a planner's, on the person's word (`request add`), not yours.

## Run states

- `claimed` / `starting` / `running`: in progress; read the session with `run log`.
- `validating`: the supervisor checks the receipt. An `already_satisfied` run stays here while the runtime runs the task's verify on a snapshot of main's head and a review job then judges the claim on that same head. A `high` pass closes the task with no commit and no ask: do not wait for one. Anything else (a `low` pass, a revise, a concern, a missing or unknown confidence, or a job that still gave no answer after its one retry) opens an `approve_close` ask carrying the review's reasons and recommendation (or why the job failed). A failed verify asks `approve_close` with only `retry` / `cancel`. If main moved before a `high` pass closes, that judgment does not close the task: the runtime verifies and judges again on the new head, and asks the person past the retry limit.
- `awaiting_integration`: accepted; under review or waiting to land. It needs you only through an ask or an attention.
- `integrating`: landing. A stale lease (`doctor`) is the `dagq-recover` skill's.
- `needs_session`: the landing failed (a conflict or a verification) or a review was sent back; the supervisor resumes its session.
- `integrated`: landed; the task is `completed`.
- `failed` / `interrupted`: the recovery job retries or resumes it, or a `decide` ask comes.
- Unfinished with `last_error` and no lease: its supervisor gave it up and recovers it once the session is gone.

Completion is decided only by these states. A Stop hook, an idle session or a `receipt.json` is not success. One run's failure never changes another run.
