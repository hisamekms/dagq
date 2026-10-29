# status, watch and run states

Read this when what `status`, `watch` or `show` says, or what to do about an attention, is unclear. Each command's flags and output: `"$DAGQ" <command> --help`.

## status

What each part of the output holds: `"$DAGQ" status --help`. What to do with it:

- No supervisor serves the queue, or the only one is stale (died or hangs): `up` on the person's word; orphaned runs go through the `dagq-recover` skill, one run at a time. A stale one whose process is still alive is not replaced: report it and let the person stop that process (`down --force` if it is the only one).
- `status` fails as `unreachable` when the queue's service does not answer: the queue cannot be read, not that nothing waits. Handle it as `watch.md` says for a watch.
- Count the runs holding a slot, waiting to land and waiting for a person from the counts `status` gives, never from the number of runs listed.
- `inbox_guardrail` names the newest inbox's `provider`, where it was chosen (`source`) and the model asked for. The inbox opened without its guardrail (`guardrail` `false`), or a Codex inbox whose guardrail hook has not run (`guardrail_by` `pre_tool_use` with `reached` still `false` after shell commands ran: `claude-and-codex.md`): tell the person and, after your handoff, have them reopen it with `dagq inbox` (`dagq-recover`'s `reference/up-down.md`, "Open the inbox again"). A `claude` or `codex` typed again in the inbox's terminal has no guardrail whatever this says.
- No watch runs: start the one of `watch.md`. When none ran for long, tell the person: asks opened meanwhile reached nobody.
- A run's phase: when it shows none, do not guess one. A person is needed only where a run waits for an answer or an attention names it. A run's session is read with `run log` (below).
- A held task needs nothing until its attention comes; what a hold does and `release`: `dagq-recover`'s `reference/hold.md`.
- The asks are cut short; read them whole with `asks --open --role inbox`.
- A red CI needs nothing from the inbox: a runtime planner registers the fix. The failing tests: `"$DAGQ" ci failures`.
- Write for people in the language it names (the `dagq` skill's section 5).
- Act on each attention by the next step it names (below); `--role inbox` keeps all of it.

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
  - `check the e2e host`: the e2e a reviewed run needs could not run several times in a row. The person checks the host in their own terminal without `DAGQ_ROLE` (`dagq doctor`). The attention clears by itself once an e2e runs; nothing to close. If the run is left as `review and integrate`, check its e2e before integrating by hand.
  - `stop the dead landing's processes`: a dead landing left processes that hold the integration slot. Show the person the pids (`dagq show TASK --full`); the person stops those pids, never by name or pattern.
- Recovery:
  - `triage by hand`: the recovery job of an ended run could not start or apply its verdict; the person decides (the `dagq-recover` skill, section 4).
  - `recover by hand`: a live run's failed recovery job that an older runtime recorded; the same section. A newer one opens a `stalled` ask instead.
  - `recover run`: a run its supervisor gave up; while its processes live, the `dagq-recover` skill. If `watch` reported it but `status` shows the run at rest, `status` wins.
  - `exit the session`: a run given up whose session's exit request could not be written. Tell the person, who ends it in their own terminal (the `dagq-recover` skill, section 2).
  - A turn that ended without a receipt is no attention: the runtime and its recovery job handle it. Do nothing about it until a `stalled` ask comes.
- Planning:
  - `plan review by hand`: the headless plan review failed and holds the proposal (the `dagq-recover` skill, section 8).
  - `check the planner`: a planner did not submit a revise in time, or a turn of a runtime planner hit its time limit; the person looks at it (`"$DAGQ" planner log ID`; section 8).
  - `goal review by hand`: a goal's review could not reach a verdict. On the person's word, `goal review ID` reruns it, or the person closes the goal or asks for tasks through a planning request (section 8). On the kind `replacement_unresolved`, a replaced task's record of what replaced it is broken and the goal cannot close as `achieved`; added work does not start its review; the person closes it `abandoned` (section 8).
  - `request a plan for the draft`: no planner adopted, dropped or asked about a draft. Tell the person; on their word, record a planning request naming it (`request add --text '<their words>' --ref task:N`), or give it a revisit time (`"$DAGQ" revisit N --at <RFC 3339 time>`, `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/register.md`, "A draft's revisit time"), or the person decides it in their own terminal.
  - A draft a planner kept has no attention and waits for a request or its revisit time; on the person's word, set, change or clear that time (`revisit --help`).
  - `request a plan for the follow-ups of goal G`: unfinished follow-ups of goal G have no settled membership, so its goal review waits. Tell the person; on their word, record a planning request naming the goal or the follow-ups (`--ref goal:G` or `--ref task:N`).
  - `request a plan for the finding`: no planner submitted, dismissed or asked about a finding. On the person's word, a planning request naming it (`--ref finding:N`), or the person submits or dismisses it in their own terminal.
  - `request a plan for the waiting tasks`: tasks of open goals wait on a task of a closed goal that will never complete. On the person's word, a planning request naming the stranded task (`--ref task:A`), or the person decides in their own terminal.
  - A request's outcomes and its planner's question: `reference/requests.md`.
- The host and its services (the person fixes them; tell them the error the attention carries):
  - `stop sccache on the host; the supervisor starts it`: the supervisor could not replace an unhealthy server. Show `doctor`'s server identity; the person stops it and leaves the start to the supervisor.
  - `install tool`: a program the run environment or the CI watch needs is missing on the supervisor's PATH. The person installs it, or asks through a planning request for a task that stops naming it.
  - `log in to gh`: the person runs `gh auth login` in their own terminal; the inbox does not log in.
  - `fix dagq.toml`: the CI watch or the container runtime does not work as configured. The person fixes it, or on their word the inbox asks through a planning request for a task that changes the configuration.
  - `fix the push command`: the host's push command dropped a message; the person fixes the command or its service.
  - `dagq service status`: the queue service was down and answers again (while it is down, `status` and `watch` fail as `unreachable` instead: `watch.md`). Show `dagq service status` and the service's log; if it is not running now, `dagq service start` (or `up`) at the person's word.
  - `read the queue broker's log and run up`: show the broker's log; at the person's word, `up`.
  - `look at the dagq podman machine's relays`: show `podman machine list` and the error; the person mends the machine.
  - `look at the dagq podman machine's memory`: a run's turn or verification has waited long for the machine's memory before its container starts. Show the error and `podman --connection dagq ps`; the person stops a container that does not end, or changes the machine's or the containers' memory in `host.toml`. The attention clears once the wait ends.
  - `restart supervisor`: no supervisor of the queue is live. `up` on the person's word. Two supervisors: `down --wait`, then `up` (`dagq-recover`'s `reference/up-down.md`).
  - `stop the stale supervisor's process`: another supervisor runs the queue, and this registration's process lives but stopped heartbeating. The person first checks that the pid is still a `dagq supervise` of this queue (`ps -p PID -o command`; a reused pid is another program: run `up` instead), then stops that pid (not `down`, which stops the live one).
  - A worker provider that cannot be used is no attention: the person acts only on a `queue_hold` ask (`reference/asks.md`).
- Holds:
  - `release or keep the hold of task N`: the hold's reminder time (`--until`) passed, or it has been on long. Tell the person the task, its reason and how long it has waited; on their word `release` it (`dagq-recover`'s `reference/hold.md`), or keep it: nothing to do.
- Notices (they do not stay in `status`; tell the person):
  - `report the update`: the supervisor updated dagq by itself, or only the plugin. Tell the person the version and what the notice says, and that sessions reopened after it load the updated plugin. Nothing to do; a failed update is an ask instead.
  - `report the supervisor's end`: a supervisor ended because its heartbeat stopped (`supervisor_heartbeat_failed`). Tell the person its `cause` (`busy`: the queue stayed locked; `unreachable`: the service did not answer; `registration_gone`; `failed`), its `failures` and `secs`. launchd starts it again; one that stays stale is the `supervisor_stale` attention above, whose `last_error` gives the same reason.
  - `report the review`, `check the failed review`: `watch.md`, "The throughput review's notice".
  - `check the failed observer`: `watch.md`, "The observer's failure notice".

## Runs waiting for a person (outside the slots)

A run whose session only waits for a person's answer leaves its slot while it waits; answering its ask is all it needs from you. A drain (`down --wait`) ends only once their asks are answered. The run of a held task waits for a `release`, not an answer: it is in `held`, not in `waiting`.

## asks

`"$DAGQ" asks` lists the asks nobody closed (`asks --help`). Show each ask's reason category with its question.

- `ask close <id>` marks an answered ask read. An unanswered ask cannot be closed: to withdraw one on the person's word, answer it first (`"$DAGQ" answer <id> --text "withdrawn: <why>"`), then close it.
- `ask` notifies nobody; your watch tells the person of new asks through the host's push, when it has one.

## watch and events

`watch` is the inbox's only wait (`watch.md`). `"$DAGQ" events --after <cursor>` returns the same attention events without waiting (`events --help`).

## Watching a background session

Every worker's and runtime planner's session runs in the background with no screen. When the person wants to see what one does, or before you report one that seems stuck:

```sh
"$DAGQ" run log RUN                # the run id, or the task id for its latest run
"$DAGQ" planner log ID             # a runtime planner's, the id from `planners`
```

Never build the log's path yourself. A following read keeps printing: run it in the background, or let the person run it. `"$DAGQ" timeline RUN` gives the times. Nothing can be typed into a session: an answer goes as its next turn.

## show

`"$DAGQ" show ID` reads one task, with long texts cut; `--full` prints them whole. `"$DAGQ" list --status in_progress` finds the tasks worth a `show`. Leave proposals from `stats` to the observer.

## goal list

When the person asks which goals go first, what moves now or what a theme holds, show them `"$DAGQ" goal list` (`--tag TAG` narrows it to a theme). Show the order as it prints, with each goal's title, priority and tags; never sort by ID or by your own reading. A goal's tasks and where each task's priority comes from: `"$DAGQ" goal show ID`. Changing a goal's priority or tags is a planner's, on the person's word (`request add`), not yours.

## Run states

A run's state needs you only through an ask or an attention: the runtime validates, reviews, lands, resumes and recovers runs itself, and an `already_satisfied` run closes its task with no commit, or asks `approve_close`. A stale landing lease (`doctor`) is the `dagq-recover` skill's.

Completion is decided only by the run's state (`integrated`, or the task `completed`). A Stop hook, an idle session or a `receipt.json` is not success. One run's failure never changes another run.
