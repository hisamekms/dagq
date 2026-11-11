# Claims held on free disk space

Read `"$DAGQ" status --role inbox` first. `claim_hold` gives when the hold started, free space, the need and its breakdown: the reserve, the new run's expectation, the moving runs' remaining space and the expectation added for a waiting landing. Read these together before judging the need. A task's older deferral is not the queue's current hold. An active `disk_relax` shows what is relaxed, who recorded it and why, its start and expiry.

Tell the person to free space on the host. Leave running runs and their leases alone; never edit the queue database or delete an active run's files. Claims resume automatically when there is room. The notice is a report, not an ask to open; an existing disk ask follows `skills/dagq-inbox/reference/asks.md`.

If the person needs an emergency relaxation, get their words about which part to loosen, the reason and how long. Only the person (a terminal without `DAGQ_ROLE`) or the inbox acting on those words uses `disk relax`; workers and planners cannot. Read `"$DAGQ" disk relax --help`, then choose only the terms the person authorized:

```sh
"$DAGQ" disk relax --no-landing-expectation --for 30m --reason "<the person's reason>"
```

This example omits the extra expectation for a waiting landing. `--expected <SIZE>` replaces the new run and landing expectation; `--no-remaining` counts moving runs' remaining space as zero. These can be combined when authorized. Choose an expectation from the breakdown and the person's judgment, never a repository-independent size. The reserve cannot be relaxed. A duration is required and limited to a day; no restart is needed. The override ends automatically at its deadline, and `status` and the inbox notice show it while active. A later relaxation replaces the earlier one.

Once space is available, return to the usual calculation early:

```sh
"$DAGQ" disk relax --clear --reason "<why the emergency relaxation can end>"
"$DAGQ" status --role inbox
```

Check that `disk_relax` and its notice have gone. Expiry and clear remove the relaxation; they do not guarantee sufficient space, so check any remaining claim hold and its breakdown too.
