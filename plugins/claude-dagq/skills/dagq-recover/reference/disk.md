# Claims held on free disk space

Read `"$DAGQ" status --role inbox` first and use the queue's current `claim_hold` even if the task shows an older deferral. When its `reason` is `disk_space`, compare the reported free space with the required space. The breakdown shows the reserve, the new run's WORKER budget W, the sum U of unused run reservations, and the shared INTEGRATOR slot's budget I. An active `disk_relax` shows what is relaxed, who recorded it and why, its start and expiry.

U includes only runs that can proceed and are in a stage with a budget. Each contributes its stage budget minus its current size, floored at zero. A run on hold or blocked by an unanswered ask, CI, its environment, or the landing branch counts only at its current size, already reflected in free space, and is excluded from the moving count. During landing or return, an unanswered ask that closes when that step completes is exempt from the answer gate. Holds still exclude those runs from the moving count and reservations. Runs waiting to return or in the return phase carry zero reservation. A run that can proceed in the INTEGRATOR stage occupies the shared slot in the disk calculation and contributes its remaining reservation to U. A blocked run in that stage leaves the slot empty for disk accounting and contributes zero to U.

For claims evaluated on their own, the free-space threshold with runs moving is the reserve plus W, U, and a separate I charge if the INTEGRATOR slot is empty, subject to any active relaxation. With zero moving runs, the threshold is just the reserve. The disk calculation gives no claim-count limit if free space is unknown and gives a limit of zero if free space falls below the threshold. With enough free space to meet the threshold, a missing W estimate gives a limit of zero while runs are moving and a limit of one with zero moving runs. A zero W gives no disk-based limit. In these cases the count calculation skips the I deduction. When the threshold is met and W is positive, the count is the space left after subtracting the reserve, applicable U, and I for an empty slot (unless that charge is relaxed), divided by W and rounded down. In this calculation, zero moving runs sets a floor of one on the resulting count.

The opening calculation covers ordinary claims when no landing candidate is up for a start on that pass. When a selected landing and a claim otherwise qualify to start on the same pass, the supervisor counts the shared reserve once, the remaining reservations of runs that can proceed, the new worker's reservation, and the selected landing's remaining reservation. The landing's remaining reservation is the INTEGRATOR budget I minus the candidate's current size, floored at zero. It fills the empty landing slot, so the calculation skips the separate I charge for that slot. If free space covers the combined need, both start on the same pass. If it falls short, the candidate with the higher effective priority goes first, with the landing first on a tie.

A different `reason` or missing disk figures can still accompany a disk shortage, for example while cleanup is reclaiming space. Check each supervisor's `claim_admission` in `status` and the host's free space. If those checks confirm a shortage, follow the steps below to free space on the host or arrange an emergency `disk relax`.

Tell the person to free space on the host. Leave running runs and their leases alone; never edit the queue database or delete an active run's files. Claims resume automatically when there is room. The notice is a report, not an ask to open; an existing disk ask follows `skills/dagq-inbox/reference/asks.md`.

If the person needs an emergency relaxation, get their words about which part to loosen, the reason and how long. Only the person (a terminal without `DAGQ_ROLE`) or the inbox acting on those words uses `disk relax`; workers and planners cannot. Read `"$DAGQ" disk relax --help`, then choose only the terms the person authorized:

```sh
"$DAGQ" disk relax --no-landing-expectation --for 30m --reason "<the person's reason>"
```

For claims evaluated on their own, `--no-landing-expectation` sets only the empty INTEGRATOR slot charge to zero in both the free-space threshold and the claim-count calculation. U, W, and the reserve are independent of this flag. An INTEGRATOR run that can proceed still contributes its remaining reservation to U, and a blocked INTEGRATOR run contributes zero either way. With zero moving runs the threshold is the reserve with or without the flag. In that case the usual count calculation still deducts I for the empty slot when free space meets the threshold and W is positive, then applies the floor of one. The flag skips the deduction and keeps that floor, so it can increase the count permitted by disk space. Unknown free space, free space below the threshold, a missing W estimate, and W of zero all take their respective count branches before the I deduction, so with zero moving runs they give the same result with either setting.

`--expected <SIZE>` overrides both W and I. `--no-remaining` counts U as zero. These can be combined when authorized. Choose an expectation from the breakdown and the person's judgment, never a repository-independent size. The reserve cannot be relaxed. A duration of up to a day is required. The relaxation takes effect without a restart. The override ends automatically at its deadline, and `status` and the inbox notice show it while active. A later relaxation replaces the earlier one.

Once space is available, return to the usual calculation early:

```sh
"$DAGQ" disk relax --clear --reason "<why the emergency relaxation can end>"
"$DAGQ" status --role inbox
```

Check that `disk_relax` and its notice have gone. Expiry and clear remove the relaxation; they do not guarantee sufficient space, so check any remaining claim hold and its breakdown too.
