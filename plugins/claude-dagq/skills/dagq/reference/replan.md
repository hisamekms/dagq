# Replanning a long-running run

Read this when a run keeps going round (reviews that keep sending it back, turns that do not shrink what is left) and someone thinks its task should go on differently: a person or the inbox asking for it, a worker proposing it on its own run, a runtime planner opened for a replanning's request, or anyone reading where one stands. The same steps hold for Claude and Codex runs. Flags and output: `"$DAGQ" replan --help`, `"$DAGQ" replans --help`. The held run, its outcomes and what a person decides: the `dagq-recover` skill's `reference/replan-held.md`.

## 1. The flow, and who moves it

Every replanning is one **operation** on a task (at most one open per task), whatever opened it:

1. **Entry** (`candidate`): a person's or the inbox's `replan request --run RUN --reason '...'` (also records the operation's planning request), a worker's `replan propose --run RUN --reason '...'` on its own run, or the runtime's detection, when `[replan]` of `dagq.toml` enables it. Another entry on the same task joins the open operation (`created: false`). With the detection off, an operation a person or a worker opened stays `candidate`: nothing diagnoses it.
2. **Diagnosis** (`diagnosing`): a read-only job judges from the run's turns by cause, the reviews that did not pass, conflicts, verification and e2e failures and the receipt whether to `continue` or `replan`. The run goes on meanwhile. Many turns alone are no reason to split; a conflict, a failed verification or the environment is fixed in the same run. A call it cannot make (low confidence, a continuation that did not shrink what is left, the automatic parks used up) is a `decide` ask to the inbox (`continue` / `replan`).
3. **Continue** (`continuing`): the operation ends and the run goes on with the same conditions; nothing was stopped.
4. **Park and save** (`pause_requested` → `quiescing` → `snapshotting` → `parked`): the supervisor fences the task, lets the running turn and jobs end without starting the next, saves the run's work (head, uncommitted and untracked changes, receipt, logs, turns) in a snapshot it checks, and gives the run's slot back. The run waits as `needs_session`. Nothing below the supervisor stops a session or touches the worktree.
5. **Plan** (`planning`): a runtime planner opened for the operation's request reads the snapshot and decides one of: continue the parked run on its saved work, split the task or redistribute its scope into children with a replacement plan, or ask a person (section 3).
6. **Plan review** (`reviewing`, `awaiting_answer` on its concern): the usual plan review plus the replacement's own checks. Its pass marks the plan approved; the children stay `submitted` and no one may `ready` them.
7. **Apply** (`applying` → `applied`): the supervisor checks the snapshot and everything again and, in one transaction, marks the original task and run `replaced`, readies the children, hands each its part of the saved work and moves every successor's dependency onto the children it needs. Or **continue after the park** (`continuing_after_park`): the run's worktree is checked against the snapshot (rebuilt when it is gone) and the runtime resumes it.

A step past the diagnosis that fails holds the operation `blocked` with its reason, the run parked and its work kept (in the snapshot, or in the untouched worktree when the save failed). A failed apply is the supervisor's to check again; any other `blocked` one waits for a person's `replan resolve` (continue, withdraw or plan again, the work kept whichever: the `dagq-recover` skill's `reference/replan-held.md`). A diagnosis that keeps failing becomes a `decide` ask instead.

## 2. What each role may do

| Role | May | Never |
| --- | --- | --- |
| Person (terminal without `DAGQ_ROLE`) | `replan request`; `replan resolve` of a `blocked` operation; answer the `decide` ask, a planner's `planner_question` and plan review's `approve_plan` | edit, `cancel`, `ready`, `integrate`, `recover`, resume or land the held task or run: refused while the operation guards it, for every role |
| Inbox | the same, on the person's word, recorded as delegated | `replan propose`; deciding for the person |
| Worker (Claude or Codex), its own run only | `replan propose --run <its run>` | `replan request`, `replan resolve`; a proposal on another run; asking a person whether to split |
| Runtime planner of the operation's request | `request decline N --continue-run`, `add` / `lint` / `submit --replacement` for the children, `planner_question` (section 3) | `replan` (`resolve` included); editing, cancelling or readying the original task, its run or worktree; `ready` of a child |
| Plan review, diagnosis and other jobs | their verdict | any command that changes state |
| Supervisor (no command) | the diagnosis's move, the park, the snapshot, the apply, the restore and resume | — |

`skills/dagq/reference/authority.md` holds the same policy. Nobody opens the queue database or edits a worktree to move an operation; the supervisor alone moves it.

## 3. The worker: propose on your own run

Propose only when the work itself is the cause: independent acceptance items that could land apart, or reviews that keep finding the same kind of gap the task's scope is too large to close. Do not propose for a conflict, a verification or e2e failure, a provider or host problem, or many turns alone.

```sh
dagq replan propose --run <your run> --reason '<which items are independent, what the reviews keep finding>' [--evidence <event id> ...]
```

Then go on with the task as before: the proposal stops nothing. If the diagnosis parks the run, the runtime stops your next turn and keeps your work; no receipt you write after that is accepted. Do not open a `worker_question` only to recommend a split.

A receipt is no replanning. A `succeeded` receipt must meet every acceptance item: a `remaining_scope` follow-up does not drop one. A `failed` receipt goes to the recovery job; it neither splits the task nor changes its conditions.

## 4. The planner of a replanning's request

Your prompt holds the operation: the acceptance items with their ordinal and hash, verify, paths, evidence, the predecessors, the snapshot's directory (read only), the turns by cause, the reviews that sent the run back and every successor. `"$DAGQ" replans <operation>` shows it with the latest plan and its problems now. Decide exactly one, with the diagnosis as your reason:

- **Continue**: the same conditions are best finished in the same run (a local defect, a conflict, the environment): `"$DAGQ" request decline N --continue-run --reason '...'`, before any submit. The runtime restores the saved work and resumes the run; it lands only through a new validation and review.
- **Split or redistribute**: add the children on the original task's goal (`add --goal G`, `--context` beginning with `from request N`), each with acceptance, `--paths`, `--verify` and `--evidence`; together they keep every verification and evidence of the original. A child waits for the original's unfinished predecessors, never for the original or a task that waits for it. Write the plan (its form is in your prompt) and `"$DAGQ" submit <child>... --replacement plan.json`. Submit refuses an item left unmapped, a weakened verification, a loop, a stale version or saved work with no clear owner; fix it and submit again.
- **Ask**: dropping an acceptance item, changing the goal's intent or throwing the saved work away is a person's: `ask --request N --kind planner_question --because scope` (`discard` for the saved work) with your recommendation and confidence. Low confidence too. Report and stop.

A revise of the replacement proposal comes back to the planner the revise opens: fix the children and the plan, `submit --proposal P --replacement plan.json`. That planner deletes an item only through `ask --task <original> --kind planner_question --because scope` (an ask about a child allows none).

The planner has no command to withdraw an operation: once it is `blocked`, a person or the inbox may withdraw it with `replan resolve --withdraw`. `proposal withdraw P` of a replacement puts the operation back to `planning`, but the request is already `proposed` and can no longer be declined: submit another plan in the same session, or the operation ends `blocked`. Never `request decline` without `--continue-run` only to drop the request: the operation then waits `blocked`, the run parked, for a person (`skills/dagq-recover/reference/replan-held.md`).

## 5. Moving a condition to a child is not deleting it

Each acceptance item of the original gets one mapping in the plan:

- `split` / `redistribute`: the item goes to one or more children's acceptance items (several children may meet it together). This keeps it; it is how a large task becomes smaller ones.
- `landed`: a completed task's landing already meets it; a child still checks it. The parked run's saved work has not landed and meets nothing.
- `deleted`: only when the person answered an operation's `scope` ask with exactly that item's `delete:<ordinal>:<hash prefix>` option; the mapping names the ask. `keep`, an answer about another item or one asked before the operation allow none.

A replacement never changes the goal's acceptance. Shrinking the goal is a separate decision of the person's (a `scope` ask, then `goal edit` on their word); a change of the goal's acceptance after the operation began makes the plan stale, and it is planned again.

Each part of the saved work (`snapshot_head`: the run's commits; `snapshot_diff`: its uncommitted changes, which sit on those commits) goes to one child with its order, and the children that need it wait for that child; the others start from `base`. A child brings its part onto its own base and verifies it as its own work: the saved work passed no review as such. Successors wait for all the children unless the plan says why fewer hold what one needs.

## 6. An example: a review that keeps sending a run back

A task has three acceptance items: (1) a new command's behaviour, (2) a migration of existing records, (3) its documents. Its run passes (1) early, but each review finds another gap in (2), and the diagnosis reads five reviews in a row that did not pass, all about (2).

1. The worker runs `replan propose --run <run> --reason 'items 1 and 3 are done; the migration (2) keeps failing review on edge cases'`, and goes on.
2. The diagnosis chooses `replan` (a unit too large, the items independent). The supervisor parks the run once its turn ends and saves its work: commit C with (1) and (3), uncommitted migration work. `replans` shows `parked`, then `planning`.
3. The planner adds child A (items 1 and 3, `carry: snapshot_head`, order 1) and child B (item 2, `carry: snapshot_diff`, order 2, depending on A since the diff sits on C, with the reviews' findings in its context), maps 1 and 3 to A and 2 to B, keeps every verify across them, and gives the task's one successor both children. It submits with `--replacement`.
4. Plan review passes. The supervisor applies it: the original task and run are `replaced`, A and B are `ready`, the successor waits for both.
5. A's run brings C's commits onto its base and lands through its own validation, review and e2e; B's run, once A landed, applies the saved migration changes and lands the same way. Only once both are `completed` may the successor be claimed, and the goal review counts the original as ended only through them; `replaced` never counts as `completed`.
