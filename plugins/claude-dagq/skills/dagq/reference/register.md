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

Pick `--verify`, `--paths`, `--evidence`, `--kind` (docs, plugin, runtime, ci) and the files named per `reference/scope.md`.

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
