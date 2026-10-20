# Hold a task, and release it

Read this to make a task wait while something in the way of its run is removed (the host, a login, an outside service), and to let it go on afterwards, perhaps on another provider or model (the `dagq-recover` skill, section 2). Flags and output: `"$DAGQ" hold --help`, `"$DAGQ" release --help`. Only the person and the inbox (on the person's word) hold and release (`skills/dagq/reference/authority.md`).

## When to hold

- The task's plan is right, but its run cannot get on now: hold it instead of answering a `stalled` ask with `stop` (which ends the session and hands the run to its recovery job), instead of moving the task back to `draft` (refused once it is `in_progress`, and it loses plan review's pass), and instead of canceling it.
- A hold is not a replanning: a run whose plan is wrong goes to the runtime's replanning (`replan request --run RUN`, `replan --help`), not to a hold.
- A hold names one task. To keep the whole queue from claiming, or to wait for a login or a usage limit, use the queue's own waits (`down`, the `queue_hold` ask), not a hold on many tasks.

## Hold

```sh
"$DAGQ" hold TASK --reason '...'                            # a run's ID names its task
"$DAGQ" hold TASK --reason '...' --until '<RFC 3339 time>'  # or @<unix seconds>: when to remind
```

- `--reason` is required and is what `status` and the reminder show: write why the task waits and what ends the wait.
- `--until` only reminds: nothing releases a hold by itself. Without it the person is reminded a day after the hold.
- The task keeps its status (`ready` stays `ready`, `in_progress` stays `in_progress`) and plan review's pass. `edit`, `set-priority`, `draft` and `cancel` keep their own rules and do not lift the hold; a task that ends (`completed`, `canceled`) leaves its hold without effect.
- One open hold per task: a second `hold` is refused. To change the reason or `--until`, release it and hold it again; between the two the task may go on (a claim, a resume or a new run), so only do it on the person's word.
- A `ready`, `submitted` or `in_progress` task can be held; a `draft` or finished one is refused.

## What a hold does in each state

The output's `state` is `settling` while what was going still goes, and `held` once the task waits.

| The task when held | What the hold does | `held` from |
| --- | --- | --- |
| `ready` or `submitted` | Not claimed | At once |
| A run's turn, validation, review or e2e goes | Let it finish (never cut short), then start no next step; the run waits with its session and worktree kept, out of its slot | When that step ended |
| A run waits for a person's answer | The answer is kept, not delivered; after the release it goes with the resume | When its session process ended |
| A run waits to land (`awaiting_integration`) | Not landed | At once |
| A run is landing (`integrating`) | The landing goes on; if it lands, the task completes | When the landing parks or waits again |
| A run stopped (`failed`, `interrupted`) | No recovery job starts; a job already going finishes, and its verdict or answer is kept, not applied | When no job goes |
| A run waits for a resume (`needs_session`) | Not resumed | At once |

A held run counts toward no slot and no `--max-waiting`, and no stall, idle or resume timeout is measured while it waits.

## Release

```sh
"$DAGQ" release TASK                                   # or a run's ID
"$DAGQ" release TASK --provider codex                  # another provider for its later turns and runs
"$DAGQ" release TASK --model MODEL                     # a model that provider takes (codex takes none)
```

- Refused with `hold_not_settled` while the hold is `settling`: wait until `status`' `held` shows `state` `held`, then release. A refusal changes nothing.
- The output's `continuation` and `decision` say how the task goes on:
  - `claim`: a task with no run is among the claims again; `--provider` and `--model` are its next claim's.
  - `continue` (`resume`): a run that waits (not `failed` or `interrupted`) on the same provider with a session goes on in it (a `needs_session` run through its resume, a run waiting to land to its landing). `--model` alone keeps the session and takes effect from the next turn. An approval or review pass from before the hold is not used for a landing after that resume, so a new `approve_landing` ask may come. A stopped run whose recovery job's verdict or answer was kept also gets `resume` when neither `--provider` nor `--model` is given (below).
  - `new_run` (`retry_inherit`): another `--provider`, a run with no session, or a stopped run (`failed`, `interrupted`) with nothing kept for it or released with `--provider` (its own included) or `--model`. The run ends; the task goes back to `ready` without plan review, and the next run carries over the run's branch and its uncommitted work (saved as a commit). This is not counted as the recovery job's automatic carry-over.
  - `new_run` (`retry_fresh`): the same, for a run with no commit of its own: the next run starts from the current main, with only its saved uncommitted work.
- A stopped run whose recovery job's verdict or answer was kept gets that applied after a release that changes neither provider nor model; it is discarded, and its ask closed with the reason, when the task or run changed meanwhile or the release chose a new run.
- A release whose work cannot be saved, or whose task's hold or run moved while it read the work (`release_read_moved`), is refused and changes nothing: read `status` again and retry.

## Reminders and the attention

`status`' `held` lists every open hold with its reason, `until`, who held it and when, `state`, the task's latest run and `overdue`. Once `until` passes (`until_passed`), or a day after a hold with none (`long_held`), the hold is the attention `release or keep the hold of task N` and `watch` tells it once a day. Bring the reason to the person: on their word `release` it, or keep it (nothing to do: it is told again a day later). A new `--until` needs a release first (above).
