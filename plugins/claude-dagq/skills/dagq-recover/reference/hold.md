# Hold a task, and release it

Read this to make a task wait while something in the way of its run is removed (the host, a login, an outside service), and to let it go on afterwards, perhaps on another provider or model (the `dagq-recover` skill, section 2). Flags and output: `"$DAGQ" hold --help`, `"$DAGQ" release --help`. Only the person and the inbox (on the person's word) hold and release (`skills/dagq/reference/authority.md`).

## When to hold

- The task's plan is right, but its run cannot get on now: hold it instead of answering a `stalled` ask with `stop` (which ends the session and hands the run to its recovery job), instead of moving the task back to `draft` (refused once it is `in_progress`, and it loses plan review's pass), and instead of canceling it.
- A hold is not a replanning: a run whose plan is wrong goes to the runtime's replanning (`replan request --run RUN`, `reference/replan-held.md`), not to a hold.
- A hold names one task. To keep the whole queue from claiming, or to wait for a login or a usage limit, use the queue's own waits (`down`, the `queue_hold` ask), not a hold on many tasks.

## Hold

```sh
"$DAGQ" hold TASK --reason '...'                            # a run's ID names its task
"$DAGQ" hold TASK --reason '...' --until '<RFC 3339 time>'  # or @<unix seconds>: when to remind
```

- `--reason` is required and is what `status` and the reminder show: write why the task waits and what ends the wait.
- `--until` only reminds: nothing releases a hold by itself.
- The task keeps its status and plan review's pass; the runtime neither claims it nor resumes, recovers or lands its run until the release. What is going (a turn, a validation, a review, a landing) is never cut short.
- One open hold per task. To change the reason or `--until`, release it and hold it again; between the two the task may go on, so only do it on the person's word.
- A task that ends closes its hold; a task opened again later does not carry it.

## Release

```sh
"$DAGQ" release TASK                                   # or a run's ID
"$DAGQ" release TASK --provider codex                  # another provider for its later turns and runs
"$DAGQ" release TASK --model MODEL                     # a model that provider takes (codex takes none)
```

- A release is refused while what was going still goes: wait until `status` shows the task held, then release. A refusal changes nothing; when it says the task moved meanwhile, read `status` again and retry.
- The output says how the task goes on: a run that waits goes on in its session when the provider stays; another provider, or a run that cannot go on, ends the run and the task's next run carries its work over. A stopped run whose recovery result the hold kept goes on with it only when the release gives neither `--provider` nor `--model`; with either, the run ends and the kept result is dropped. An answer kept for a waiting run is lost when the release starts a new run. A resumed run's landing goes through review again, so a new `approve_landing` ask may come.

## Reminders and the attention

Once the reminder time (`--until`) passes, or after a long hold with none, the hold is the attention `release or keep the hold of task N`. Bring the reason to the person: on their word `release` it, or keep it (nothing to do). A new `--until` needs a release first (above).
