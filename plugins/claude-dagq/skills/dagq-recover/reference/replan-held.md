# A run held by a replanning: read it, decide, recover

Read this when `status`, `doctor`, `show` or `list` shows a task or run under a replanning operation, when its `decide`, `planner_question` or `approve_plan` ask reaches the inbox, or when the person asks to replan a run. The flow, the roles and how a plan maps the conditions: `skills/dagq/reference/replan.md`. Everything here is done on the person's word.

## Ask for a replanning

```sh
"$DAGQ" replan request --run RUN --reason '<the person's words>' [--evidence <event id> ...]
```

It prints the operation, whether it was `created` and its planning request. A request on any run of a task whose operation already guards it joins that operation; a request on an earlier run while one is open on the latest is refused. When `[replan]` of `dagq.toml` does not enable the detection, the operation stays `candidate` and nothing diagnoses it: tell the person.

A replanning is not a hold (`reference/hold.md`): a hold keeps the plan and waits; a replanning may change it.

## Read where it stands

```sh
"$DAGQ" replans            # open operations; --all for the ended ones
"$DAGQ" replans ID         # one, with its latest replacement plan and that plan's problems now
"$DAGQ" status             # replans: open, guarding, problems, detection, counts
"$DAGQ" timeline RUN       # the run's turns by cause and its replanning steps
```

- `candidate`, `diagnosing`: the run goes on as usual.
- `continuing`: the diagnosis (or the person's `continue`) ended it; the run went on.
- `pause_requested`, `quiescing`, `snapshotting`: the supervisor is stopping the run at a turn's end and saving its work. Wait.
- `parked`, `planning`, `reviewing`, `awaiting_answer`, `applying`: the work is saved, the run waits `needs_session`, and the task is guarded: `edit`, `cancel`, `ready`, `integrate`, `recover`, a dependency change, a resume and a landing are refused for every role. A review verdict or receipt from before the park is kept but starts no revise, ask or landing (`replan_result_withheld`). Do not resume the run (`reference/resume.md`).
- `applied`: the task and its run are `replaced`, never `completed`; `replaced_by` names the children. Follow the children as any task. The successors wait for the children they need, each `completed`.
- `continuing_after_park`: the run resumes on its saved work (`replan_restored` when its worktree was rebuilt), then lands only through a new validation and review; an earlier approval does not count.
- `blocked`: a step failed; `status`'s `problems` names it with `return_phase` and the reason. The run stays parked; the saved work is kept, or, when the save itself failed, the worktree is left as it was.

## What a person decides

- **`decide` ask by the diagnosis** (`continue` / `replan`): answer with the option; the supervisor checks the premise again and applies it, or closes the ask when things moved on. The inbox's steps: `skills/dagq-inbox/reference/asks.md`.
- **`planner_question` about the request** (`scope` or `discard`): dropping an acceptance item, changing the goal's intent, throwing the saved work away. To drop an item, the answer is exactly its `delete:<ordinal>:<hash prefix>` option; `keep` keeps it. The runtime delivers the answer to the planner.
- **`approve_plan`** (plan review's concern on the replacement): `ready` approves the plan if it still holds (otherwise it goes back to the planner with the problems); `send_back: <reason>` returns it to a planner; `cancel` drops the proposal and its children; no planner is opened again, so the operation ends `blocked` with the run still parked (below). None of them cancels the original task.
- **Shrinking the goal** is not part of a replacement: a separate `scope` decision, then `goal edit` on the person's word. A plan made before that change is stale and is planned again.

Never answer on the person's behalf, and never carry out a decision by editing the task, its run, its worktree or the queue database.

## Recover a blocked operation

Read the reason first (`replans ID`, `status`'s `problems`, `run log RUN`). Then by its kind:

- **The apply was refused by something that passes** (a write, a lease): the supervisor checks again at each planning pass and applies once nothing refuses it. Wait; keep a supervisor running.
- **The plan no longer holds** (a version, a child, a successor already started, a loop): the supervisor sends the children back to draft and the operation back to `parked` with a new request; a new planner decides. Nothing to do.
- **Anything else**: the snapshot does not match, the save failed, a process the run left still writes in its worktree, the restore failed, the planner declined without `--continue-run`, the request ran out of planners, or the replacement proposal was withdrawn or canceled on `approve_plan` with no planner left ("the inbox decides"). No command moves such an operation on yet: it stays `blocked`, the task held, the work kept and nothing overwritten. Show the person the reason; a left process is named there with its pid, which the person may stop in their own terminal. A new `replan request` joins the same operation, and `request add` makes an ordinary plan that does not touch it.

Never repair a blocked operation by opening the database, deleting the snapshot or its pinned ref, editing or removing the old worktree, or recreating the task: the operation is the only record of the saved work and the conditions.
