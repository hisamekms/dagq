# Registering goals and tasks: the details

What `SKILL.md` section 2 names in short. `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`.

## Goal fields and states

Collect, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed document, relative to the repository root).

A goal has no verification commands; a goal-level check is a final task depending on the others. It is draft or open: the tasks of a `goal add --draft` goal are never claimed, even when `ready`. Adopt it by submitting it (`submit --goal ID`; a `pass` lifts the draft), or reject it with `goal close ID --verdict abandoned`. The observer's findings, and how they become proposals: `reference/observer.md`.

## Task fields

Split the goal into tasks, each one session in one worktree. Collect per task:

- title (one line)
- description (what to change and where; for runtime, the files it mainly touches)
- acceptance (how a reviewer decides it is done)
- verification commands (run by `integrate` after its rebase; repeat `--verify`)
- dependencies (tasks that must be `completed` first, repeat `--depends-on`; to wait for another goal, `--depends-on-goal ID`: claimed only once that goal is closed `achieved`, `reference/inspect.md`)
- `--context` (why it exists, what to read first)
- `--evidence` (receipt checks to report `passed` with evidence: `tests`, `e2e`, `subagent_review`). A missing one parks the run (`evidence_missing`).
- `--paths GLOB` (repeatable) limits what a task may change: a run changing more parks (`scope_violation`).
- `--change CHANGE`: the kind of change the task makes (a lowercase label, ADR-t980-1). When the repository's `dagq.toml` lists `[tasks] changes`, give one of them: `add` / `edit` refuse another value, and `lint` (`missing_change`) and `submit` refuse a task without one. Without the list it is optional.

Pick `--verify`, `--paths`, `--evidence`, `--change` and the files named per `reference/scope.md`. `--kind LABEL` still exists but is going away (ADR-t980-1): give it to no new task; `--change` and the area (read from the landed diff) replace it.

## From draft to ready

A one-shot task omits `--goal`. `add` makes a `draft`, never claimed. `submit` (refusing what `lint` rejects) makes the tasks `submitted`, never claimed, in one proposal owned by this session. Plan review checks each proposal in turn:

- `pass` makes its tasks `ready`.
- `revise` returns them to `draft` with reasons for their planner to fix and `submit --proposal ID` again.
- `concern` asks the person through the inbox.

`proposal list` / `proposal show ID` read proposals. Its planner withdraws one with `proposal withdraw ID` (a dropped plan, or `EmptyProposal`): it ends `canceled`, its submitted tasks `draft` (`reference/inspect.md`). Only plan review readies a task; `ready --bypass-review` skips it on a person's explicit word. `candidates` lists claimable ready tasks; a ready one missing from it is blocked (`show ID`).

## Changing a registered task

`edit` changes a draft or submitted task, `draft ID` takes a ready or submitted task back, `cancel ID` drops it (`--duplicate-of X` for a duplicate), `dependency add|remove TASK PREDECESSOR` (or `--goal ID`) changes prerequisites. `set-goal`, `goal edit`, `edit`: `reference/inspect.md`; `set-paths`: `reference/scope.md`.

## Priority

`--priority LEVEL` (default `normal`; `set-priority TASK LEVEL` while `draft` or `ready`) orders claiming:

- `interrupt`: a rare cut-in, never routine
- `urgent`: a defect stopping operation
- `high`: a prerequisite of other work
- `normal`
- `low`: deferred

Claim order, and urgency without drafting or bending dependencies: `reference/inspect.md`.

## For a planner: checks, traffic control, revise and runtime planners

What the `dagq-planner` skill's sections 1 to 3 name in short.

**Before and after `add`.** Name in a runtime task's description the files it mainly touches: a forecast plan review and `related` match, not a limit, never `--paths` (`reference/scope.md`). To wait for another goal, depend on the goal (`--depends-on-goal ID`), not its last task, which follow-up drafts outlive; inside a goal, task dependencies (`reference/inspect.md`). A task needing paths outside its `--paths` (a `failed` receipt names them): `set-paths` before a claim, else add it again wider. Before each `add`, `"$DAGQ" search` the problem's file and test names and title words; after it, `"$DAGQ" related ID`, reading only the top candidates in full (`show ID --full`). Cancel a duplicate or work already done: `"$DAGQ" cancel ID --duplicate-of X`.

**Traffic control is plan review's.** It checks the proposal against other proposals and ready tasks (duplicates, done work, conflicts, same-file dependencies) and adds dependencies, lowers priorities or cancels an obvious duplicate itself. Beyond `search` / `related`, a planner does not take stock of other drafts, or re-wire or park other tasks. Set a priority only when the person says a task goes first or can wait (`add --priority`, `set-priority`); never draft other tasks or bend dependencies to hurry one.

**A revise.** The supervisor types "Plan review sent proposal N back" with the reasons into the planner's terminal (a runtime planner: in its initial prompt). The proposal's drafts are `draft` again (tasks plan review reopened stay `submitted` and go again as they are). Fix what the reasons point at with `edit`, `dependency`, `add` or `cancel`, `lint --proposal N`, then `"$DAGQ" submit --proposal N`. If that fails with `EmptyProposal` (a bypass or `cancel` left no draft), or the plan is dropped: `"$DAGQ" proposal withdraw N` (it ends `canceled`, its `submitted` tasks `draft`). A fix that changes the plan's intent (acceptance, scope, the relation to the goal):

- **Opened by a person**: ask the person in that terminal before changing it. A revise left unanswered is told to the inbox (`check the planner`); nothing closes the workspace.
- **Opened by the runtime**: `"$DAGQ" ask --task ID --kind planner_question --because scope --question '...'` (everything the person needs, the recommendation), report and stop. The answer arrives as `answer to ask <id>: ...`; apply it.

A ready task plan review must change is moved back to `submitted` (never claimed) into a proposal of its own for a runtime planner, with the reasons: fix it and `submit --proposal N`. A person's concern (`approve_plan`) goes to the inbox, never to a planner; its `send_back` returns as a revise.

**A runtime planner for a draft** first runs `related ID` on it (and `search`), then does one of what its prompt lists: adopt (complete it with `edit`, `lint`, `submit`), drop (`cancel` and `note`; `cancel --duplicate-of X` when a candidate already covers it), or ask (`planner_question` with `adopt` / `cancel` / `keep_draft`). It reports briefly and stops; the runtime ends the session. A draft kept with `keep_draft` or left undecided (`decide the draft in a planner`) is decided the same way by a person-opened planner with the person.

**A runtime planner for a finding** (a `kpi` one too: tasks `normal` or lower) checks `search` / `related` first, then does one: `submit ... --finding N` of tasks on an open goal or a new draft goal (no person's approval needed); `finding dismiss N --reason`; or, only for what a person must decide, `ask --finding N --kind planner_question` (`reference/observer.md`).
