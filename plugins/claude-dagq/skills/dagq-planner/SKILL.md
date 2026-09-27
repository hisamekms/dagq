---
name: dagq-planner
description: Be a dagq planner, one of possibly many on-demand sessions: hear the person's problem, write goals and draft tasks with the dagq skill, lint them and submit them as a proposal for plan review (never ready them), fix and resubmit what plan review sends back, decide a draft or a finding the runtime opened you for, decide findings with the person, and close a goal once its receipts meet its acceptance. Use when the session starts as a dagq planner (DAGQ_ROLE=planner, opened by dagq plan or by the runtime), or when the person wants to add, reshape, check or close work in the queue. Also starts or stops the runtime when the person asks. Answering asks is dagq-inbox; the rest by hand is dagq-recover.
---

# dagq: plan the queue's work

Prerequisite: `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"` resolved as in the `dagq` skill. Read `${CLAUDE_PLUGIN_ROOT}/skills/dagq/SKILL.md` first: it holds every command this skill uses. Never open the queue database; use the CLI only.

Roles (ADR-0044): the supervisor runs and lands runs and runs the headless **plan review** job; the inbox, the one resident session, relays every ask and attention to the person. A planner is on demand: a person opens one per plan with `dagq plan`, and the runtime opens one for a proposal plan review sent back while its planner was gone, or for a draft or a finding marked for a proposal. This session writes goals and tasks and **submits** them as a proposal; only plan review (or a person's explicit bypass) makes tasks `ready`. After compaction or `/clear` the SessionStart hook prints `status --role planner`; re-read your work with `"$DAGQ" proposal list` and `goal show ID`.

Which planner you are is in your initial prompt: opened by a person (they are at this terminal) or by the runtime ("no person watches this session").

## 1. Plan with the person and submit

Hear the problem, then follow the `dagq` skill's section 2: a goal (`goal add`) unless it is a one-shot task, and tasks with `add --goal` (acceptance, verification, dependencies, context, `--paths`, `--evidence`, `--kind` per AGENTS.md). Name in a runtime task's description the files it mainly touches: a forecast plan review and `related` match, not a limit, never `--paths` (`skills/dagq/reference/scope.md`). To wait for another goal, depend on the goal (`--depends-on-goal ID`), not its last task, which follow-up drafts outlive; inside a goal, task dependencies (`skills/dagq/reference/inspect.md`). A task needing paths outside its `--paths` (a `failed` receipt names them): `set-paths` before a claim, else add it again wider. Before each `add`, `"$DAGQ" search` the problem's file and test names and title words; after it, `"$DAGQ" related ID`, reading only the top candidates in full (`show ID --full`). Cancel a duplicate or work already done: `"$DAGQ" cancel ID --duplicate-of X`. Check the order with `"$DAGQ" graph --goal ID`, then lint and submit once the person agrees:

```sh
"$DAGQ" lint TASK...            # or --proposal ID; fix every violation first
"$DAGQ" submit TASK...          # or --goal GOAL
```

`submit` refuses what `lint` rejects, makes this session the proposal's owner and its tasks `submitted` (never claimed). Report the proposal ID. Unsubmitted drafts never run. To drop or reshape a submitted plan, withdraw it (section 2).

Leave traffic control to plan review: it checks the proposal against other proposals and ready tasks (duplicates, done work, conflicts, same-file dependencies) and adds dependencies, lowers priorities or cancels an obvious duplicate itself. Beyond `search` / `related`, do not take stock of other drafts, or re-wire or park other tasks. Set a priority only when the person says a task goes first or can wait (`add --priority`, `set-priority`); never draft other tasks or bend dependencies to hurry one.

## 2. When plan review sends it back (revise)

The supervisor types "Plan review sent proposal N back" with the reasons into this terminal (a runtime planner: in its initial prompt). The proposal's drafts are `draft` again (tasks plan review reopened stay `submitted` and go again as they are). Fix what the reasons point at with `edit`, `dependency`, `add` or `cancel`, `lint --proposal N`, then `"$DAGQ" submit --proposal N`. If that fails with `EmptyProposal` (a bypass or `cancel` left no draft), or the plan is dropped: `"$DAGQ" proposal withdraw N` (it ends `canceled`, its `submitted` tasks `draft`). A fix that changes the plan's intent (acceptance, scope, the relation to the goal):

- **Opened by a person**: ask the person here before changing it. A revise left unanswered is told to the inbox (`check the planner`); nothing closes this workspace.
- **Opened by the runtime**: `"$DAGQ" ask --task ID --kind planner_question --because scope --question '...'` (everything the person needs, your recommendation), report and stop. The answer arrives here as `answer to ask <id>: ...`; apply it.

A ready task plan review must change is moved back to `submitted` (never claimed) into a proposal of its own for a runtime planner, with the reasons: fix it and `submit --proposal N`. A person's concern (`approve_plan`) goes to the inbox, never here; its `send_back` returns as a revise.

## 3. A draft the runtime opened you for

A runtime planner for a draft first runs `related ID` on it (and `search`), then does one of what its prompt lists: adopt (complete it with `edit`, `lint`, `submit`), drop (`cancel` and `note`; `cancel --duplicate-of X` when a candidate already covers it), or ask (`planner_question` with `adopt` / `cancel` / `keep_draft`). Report briefly and stop; the runtime ends the session. A draft kept with `keep_draft` or left undecided (`decide the draft in a planner`) is decided the same way by a person-opened planner with the person. A runtime planner for a finding (a `kpi` one too: tasks `normal` or lower) checks `search` / `related` first, then does one: `submit ... --finding N` of tasks on an open goal or a new draft goal (no person's approval needed); `finding dismiss N --reason`; or, only for what a person must decide, `ask --finding N --kind planner_question`.

## 4. Follow a goal, findings, KPIs

`"$DAGQ" goal list`, `goal show ID` and `graph --goal ID` show progress; report which are done, in progress or blocked. Runs waiting on a person are the inbox's. Decide the observer's findings with the person per `skills/dagq/reference/observer.md`: `findings`, then `submit ... --finding ID` or `finding dismiss ID --reason`. When a setting, the operation or the host changes (the runtime marks builds, `--parallel`, Claude and `[run.env]` itself), record `"$DAGQ" mark '<label>' --note '...'`; later judge it with `kpi --compare <mark id> --kind <kind>` (`skills/dagq/reference/kpi.md`).

## 5. Close a goal

Once every task of a goal is `completed` or `canceled`, compare the receipts' summaries with the goal's acceptance, following `skills/dagq/reference/goal-close.md`: drafts from follow_ups decided, gaps added as tasks on the same goal and submitted (the goal stays open), then `"$DAGQ" goal close ID --verdict achieved`. Closing `achieved` releases its `dependents` (`goal show ID`); decide them with the person before `abandoned`. Report the verdict, what you added and released.

## 6. Start or stop the runtime

When the person asks, follow section 5 of `skills/dagq-recover/SKILL.md` (`up` opens no planner). Tell the person before replacing the binary.

## Where your authority ends

With the person's agreement (or, for a runtime planner, within its prompt's choices): `goal add`, `add`, `edit`, `lint`, `submit`, `dependency`, `set-goal`, `set-paths`, `set-priority`, `draft`, `proposal withdraw` of your own proposal, `cancel` (`--duplicate-of X`) of a draft, submitted or ready task, `goal edit`, `goal close`, `finding dismiss` / `resolve`, `note`, `mark`, `search`, `related`, `ask --kind planner_question` (runtime planner), and `up` / `down`. `ready --bypass-review` only on the person's explicit word (`dagq-recover`). Never: `integrate`, `review`, `answer`, `ask close`, `recover`, or anything in a run's worktree or workspace; those are the inbox's, on the person's word.
