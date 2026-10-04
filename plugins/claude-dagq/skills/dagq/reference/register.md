# Registering goals and tasks: the details

What `SKILL.md` section 2 names in short. `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`.

## Goal fields and states

Collect, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed document, relative to the repository root).

A goal has no verification commands; a goal-level check is a final task depending on the others. It is draft or open: the tasks of a `goal add --draft` goal are never claimed, even when `ready`. Adopt it by submitting it (`submit --goal ID`; a `pass` lifts the draft), or reject it with `goal close ID --verdict abandoned`. The observer's findings, and how they become proposals: `reference/observer.md`.

## Task fields

Split the goal into tasks, each one session in one worktree. A task's prompt shows its goal, its dependencies' receipts and siblings in progress, so siblings agree on names. Collect per task:

- title (one line)
- description (what to change and where; for a task without `--paths`, the files it mainly touches). A task that changes behavior or a specification names here or in `--context` the related documents (repository-relative path and section) and why each needs updating
- acceptance (how a reviewer decides it is done). Split it into numbered items a reviewer can check one by one: the worker maps each item to what meets it before its receipt. For a behavior change, make the changed behavior and its related documents checkable for agreement. For a measuring task, state the number of rounds (and whether they alternate), the table's columns, the formula of each value (what is divided by what, which interval of waiting is subtracted), and where the evidence lives (a document section, CSV, script, the command and its time bounds such as `--since` / `--until`); for one that does not fit (rounds of a one-time read), say why. A criterion widened to "anything of the same form" names the grep or list that bounds it. A worker cannot create or drive a throwaway queue (`init`, `add`, `up`, or `doctor` / `stats` / `kpi` run by hand against a real scratch queue): authorization gives it no `queue.admin`, and its state-changing commands reach only its own run and task. So write no such hand steps into a worker's acceptance: make the check a test (an integration test fixture or e2e), or keep it outside the task as a check for the person or the inbox.
- verification commands (run by `integrate` after its rebase; repeat `--verify`)
- dependencies (tasks that must be `completed` first, repeat `--depends-on`; to wait for another goal, `--depends-on-goal ID`: claimed only once that goal is closed `achieved`, `reference/inspect.md`)
- `--context` (why it exists, what to read first)
- `--evidence` (receipt checks to report `passed` with evidence: `tests`, `subagent_review`; a missing one parks the run, `evidence_missing`). `e2e` is not a receipt check (ADR-t1233-2): no worker runs it, and `--evidence e2e` makes the runtime run the e2e on the host after the review passes, whatever the diff. When `dagq.toml` has `[e2e] paths`, a run whose diff touches them gets that e2e too, so a task needs `--evidence e2e` only when the real cmux must check it whatever its diff (`reference/scope.md`).
- `--paths GLOB` (repeatable) limits what a task may change: a run changing more parks (`scope_violation`). Include the documents it needs, but it is an allowance, not a list of documents that must change: do not narrow the code it may change to fit the document list.
- `--change CHANGE`: the kind of change the task makes (a lowercase label, ADR-t980-1). When the repository's `dagq.toml` lists `[tasks] changes`, give one of them: `add` / `edit` refuse another value, and `lint` (`missing_change`) and `submit` refuse a task without one. Without the list it is optional.

Pick `--verify`, `--paths`, `--evidence`, `--change` and the files named per `reference/scope.md`. The task's `--kind` was removed (ADR-t980-1): `--change` and the area (read from the landed diff) replace it, and `add` / `edit` refuse `--kind`.

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

## For a planner: checks, traffic control, revise, requests and runtime planners

What the `dagq-planner` skill's sections 1 to 3 name in short.

**Before and after `add`.** Name in the description of a task without `--paths` the files it mainly touches: a forecast plan review and `related` match, not a limit, never `--paths` (`reference/scope.md`). To wait for another goal, depend on the goal (`--depends-on-goal ID`), not its last task, which follow-up drafts outlive; inside a goal, task dependencies (`reference/inspect.md`). A task needing paths outside its `--paths` (a `failed` receipt names them): `set-paths` before a claim, else add it again wider. Before each `add`, `"$DAGQ" search` the problem's file and test names and title words; after it, `"$DAGQ" related ID`, reading only the top candidates in full (`show ID --full`). Cancel a duplicate or work already done: `"$DAGQ" cancel ID --duplicate-of X`.

**Traffic control is plan review's.** It checks the proposal against other proposals and ready tasks (duplicates, done work, conflicts, same-file dependencies) and adds dependencies, lowers priorities or cancels an obvious duplicate itself. Beyond `search` / `related`, a planner does not take stock of other drafts, or re-wire or park other tasks. Set a priority only when the person says a task goes first or can wait (`add --priority`, `set-priority`); never draft other tasks or bend dependencies to hurry one.

**A revise.** "Plan review sent proposal N back" with the reasons reaches the proposal's planner: typed into its terminal or handed as its next turn while it is open, or in the initial prompt of a runtime planner opened for it once it is gone. The proposal's drafts are `draft` again (tasks plan review reopened stay `submitted` and go again as they are). Fix what the reasons point at with `edit`, `dependency`, `add` or `cancel`, `lint --proposal N`, then `"$DAGQ" submit --proposal N`. If that fails with `EmptyProposal` (a bypass or `cancel` left no draft), or the plan is dropped: `"$DAGQ" proposal withdraw N` (it ends `canceled`, its `submitted` tasks `draft`). A fix the reasons name is made as they say, without asking (the `dagq-planner` skill's Basic policy, ADR-t451-1). A fix that changes the plan's intent (acceptance, scope, the relation to the goal) goes up only when the queue, the repository, the ADRs and the person's precedents cannot settle it:

`"$DAGQ" ask --task ID --kind planner_question --because scope --question '...'` (everything the person needs, the recommendation), report and stop. The answer arrives as `answer to ask <id>: ...`; apply it. A revise left unanswered is told to the inbox (`check the planner`).

People no longer open planners (ADR-t1394-1: `dagq plan` is refused). A person's planner opened before keeps running until it ends: once its agent has exited (`/exit`, or `planner send --key exit`), the supervisor closes the workspace and its record 60 seconds later, recorded as `planner_closed` (ADR-t1300-1).

A ready task plan review must change is moved back to `submitted` (never claimed) into a proposal of its own for a runtime planner, with the reasons: fix it and `submit --proposal N`. A person's concern (`approve_plan`) goes to the inbox, never to a planner; its `send_back` returns as a revise.

**A runtime planner for a draft** first runs `related ID` on it (and `search`), then does one of what its prompt lists: adopt (complete it with `edit`, `lint`, `submit`), drop (`cancel` and `note`; `cancel --duplicate-of X` when a candidate already covers it), or ask (`planner_question` with `adopt` / `cancel` / `keep_draft` and its recommendation) only for what the Basic policy raises: a person's `scope` or `discard` it cannot settle, a call it is not confident of, or a follow_up past ADR-t808-1's limit (depth 3 or more; a missing, closed or unknown source goal at registration; or no current goal or a closed current goal), which always waits for the person's adopt. `set-goal` and `judge-follow-up` keep registration facts and depth, and never count as adoption. An existing person's adopt remains valid. Record why it adopted or dropped in a `note` or the task's `context`. It reports briefly and stops; the runtime ends the session. A draft kept with `keep_draft` or left undecided (`request a plan for the draft`) stays a draft until the inbox, on the person's word, records a planning request that names it (`request add --ref task:N`); that request's runtime planner decides it the same way, asking the person only what the Basic policy raises. The person may instead decide it in their own terminal (no `DAGQ_ROLE`).

**A runtime planner for a request** (the person's words the inbox recorded with `request add`, ADR-t1394-1) reads the words, the inbox's note, what they refer to (asks, tasks, runs, events, findings, goals) and the goals they lead to, looks for work that already covers them, and does one: plans it, declines it (`request decline N --reason '<why>'`: done already, a duplicate in flight, or not plannable as asked), or asks (`ask --request N --kind planner_question` with `plan` / `decline` and its recommendation) only for what the Basic policy raises. Up to 3 planners are opened for a request before it is `exhausted`; the inbox tells the person each outcome (`skills/dagq-inbox/reference/requests.md`).

To plan it (the `dagq-planner` skill's section 1): a goal (`goal add --draft`) unless it is a one-shot task or fits an open goal, and tasks with `add --goal`: acceptance, verification, dependencies, `--context` beginning `from request N`, `--paths`, `--evidence` and `--change` per the repository's rules (`reference/scope.md`), `e2e` from the diff and `[e2e] paths`, not a blanket `--evidence e2e` (above), and no throwaway-queue hand steps in a worker's acceptance ("Task fields"). After each `add`, `"$DAGQ" related ID`, cancelling a duplicate with `cancel ID --duplicate-of X`. Check the order with `"$DAGQ" graph --goal ID`, then `lint` and `submit`: `submit` refuses what `lint` rejects and makes the planner the proposal's owner, its tasks `submitted` and the request `proposed`, with no person's approval needed, even for a new goal. Report the proposal ID. The worker's provider and route: no flag (Claude headless) unless the person's words ask for `--provider codex` or `--interactive` (`reference/provider.md`, "Which to choose").

**A runtime planner for a finding** (a `kpi` one too: tasks `normal` or lower) checks `search` / `related` first, then does one: `submit ... --finding N` of tasks on an open goal or a new draft goal (no person's approval needed); `finding dismiss N --reason`; or, only for what a person must decide, `ask --finding N --kind planner_question` (`reference/observer.md`).
