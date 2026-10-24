# Registering goals and tasks: the details

What `SKILL.md` section 2 names in short. `DAGQ="${CLAUDE_PLUGIN_ROOT}/bin/dagq"`. Each command's flags and values: `"$DAGQ" <command> --help`.

## Goal fields and states

Collect, asking only for what is missing: title (the problem, one line), description (what is wrong today and what the repository looks like when solved), acceptance (how the whole goal is judged after every task landed), constraints (naming, boundaries, what not to do, shared by every task), doc (a committed document), priority and tags.

- Priority: the source of its tasks' priority ("Priority"). For a request with a priority, leave `--priority` out: the runtime gives the person's. Otherwise pick it by what waiting for the whole goal costs (`reference/inspect.md`) or as the repository's rules say. Never copy a priority out of the request's words or a `context`.
- Tags: what the goal is about, from the vocabulary the repository's `dagq.toml` lists, when it lists one.

A goal has no verification commands; a goal-level check is a final task depending on the others. The tasks of a draft goal are never claimed: adopt it by submitting it (`submit --goal ID`), or reject it with `goal close ID --verdict abandoned`. Findings and how they become proposals: `reference/observer.md`.

## Task fields

Split the goal into tasks, each one session in one worktree. Collect per task:

- title (one line), starting with one of the repository's title prefixes when it lists them.
- description (what to change and where; for a task without `--paths`, the files it mainly touches). A task that changes behavior names here or in `--context` the related documents (path and section) and why each needs updating. Before `submit`, read `lint`'s hints and add each named section that applies, or say why it needs no update.
- acceptance, in numbered items a reviewer can check one by one. For a behavior change, make the changed behavior and its documents checkable for agreement. For a measuring task, state the rounds, the table's columns, each value's formula and where the evidence lives, or why that does not fit. A criterion widened to "anything of the same form" names the grep or list that bounds it. A worker cannot create or drive a throwaway queue: write no such hand steps into its acceptance; make the check a test, or a check for the person or the inbox outside the task.
- verification commands (`--verify`, run by `integrate` after its rebase)
- dependencies (`--depends-on`; to wait for another goal, `--depends-on-goal ID`, `reference/inspect.md`)
- `--context` (why it exists, what to read first)
- `--evidence` (receipt checks to report `passed` with evidence). No worker runs the e2e: give `--evidence e2e` only when the real processes must check it whatever its diff (`reference/scope.md`).
- `--paths GLOB` limits what a task may change. Include the documents it needs; it is an allowance, not a list of documents that must change.
- `--change CHANGE`: the kind of change, one of the repository's when it lists them.
- `--execution-class spike`: the run investigates a premise by experiment, prototype, build or measurement instead of implementing. Its description holds its question, impact, method, limit, evidence and what each verdict leads to; its acceptance is a verdict with grounds (`reference/spike.md`).

Pick `--verify`, `--paths`, `--evidence`, `--change` and the files named per `reference/scope.md`.

## From draft to ready

A one-shot task omits `--goal`. `add` makes a `draft`; `submit` (refusing what `lint` rejects) puts the drafts in one proposal for plan review. `pass` readies them; `revise` returns them to `draft` with reasons to fix and `submit --proposal ID` again; `concern` asks the person through the inbox. Withdraw a dropped plan with `proposal withdraw ID` (`reference/inspect.md`). Only plan review readies a task; `ready --bypass-review` skips it only on a person's explicit word. A ready task missing from `candidates` is blocked (`show ID`).

## Changing a registered task

`edit` changes a draft or submitted task, `draft ID` takes a ready or submitted task back, `cancel ID` drops it ("Cancel reasons"), `dependency add|remove` changes prerequisites. `set-goal`, `goal edit`, `edit`: `reference/inspect.md`; `set-paths`: `reference/scope.md`.

## Cancel reasons

`cancel ID` needs `--reason CODE` or `--duplicate-of X`. Choose by where the task's content is now. When another task X holds it, name X: `--duplicate-of X` alone for an open duplicate (`duplicate`) or work X already landed (`already_done`), `--reason absorbed --duplicate-of X` when X took the content, `--reason re_registered --duplicate-of X` when X replaces it under a new ID. Otherwise: `superseded` (a decision changed; name it in `--note`, and X if a successor exists), `not_worth` (right, but not worth its change and checks), `decision_moot` (a question already answered or no longer asked), `not_repo_work` (work outside the repository, for a person or the inbox), `stale` (its premise is gone, with no successor), `other` (`--note` says why). `answered_cancel` and `unrecorded` are the runtime's own; a person's `cancel` you carry out takes the code that fits it.

## Priority

Five levels order claiming (`interrupt`, `urgent`, `high`, `normal`, `low`; when to use each: `reference/inspect.md`). The goal's priority is the source: a task without its own takes its goal's. Give a task its own (`set-priority`) only to raise or lower a few tasks of a goal; otherwise change the goal's (`goal edit ID --priority`). A priority a person set on the task itself stays on any move; what else a moved task keeps: `set-goal --help`.

**By where a task or goal comes from**. Human-made is what a request's planner makes or what the person or the inbox adds; AI-made is what a finding's, a follow_up's, a goal gap's or a draft's planner or the runtime makes. A `context` or description (`from request N` included) is not its origin.

- Plan review does not change a priority a person set, nor the priorities or goals of human-made work: it raises a `concern`. A planner that doubts a person's priority asks a `planner_question` and does not work around the CLI's refusal.
- A new goal of a request without a priority, and an AI-made new goal, take one from the repository's guide (say so in the description).
- An AI-made task takes no priority of its own, and goes into an existing goal only when that goal's acceptance needs it, else into a new goal or none. A follow_up with a source goal goes by "A follow_up's membership" below.
- When an inherited priority looks too high, fix the membership or the goal's priority, not the task's.

## For a planner: checks, traffic control, revise, requests and runtime planners

What the `dagq-planner` skill's sections 1 to 3 name in short.

**Premises and Spikes.** Before you split the work, settle the unconfirmed premises a short read answers, move only those needing an experiment into Spikes, submit the Spikes alone and stop; a new planner decides the next step from the Spike's verdict (`reference/spike.md`).

**Wait for the installed build.** When a task needs the installed binary to contain a prerequisite's landing, use `--wait-for-build` with `--depends-on ID`, not a build gate in `--verify`. Not for documentation-only prerequisites.

**Before and after `add`.** Depend only on a content prerequisite: work that must land before this task can be done. Never add a dependency, or ask, only to keep tasks that touch the same files apart: the claim deferral and the landing handle that. Record each dependency's kind and reason in a `note` or `context`. To wait for another goal, depend on the goal, not its last task. A task needing paths outside its `--paths`: `set-paths` before a claim, else add it again wider. Before each `add`, `search` the problem's file and test names and title words; after it, `related ID`, reading only the top candidates in full. Cancel a duplicate or done work: `cancel ID --duplicate-of X`.

**Traffic control is plan review's.** It checks proposals against each other and against ready tasks, adds only a dependency a content prerequisite needs, and cancels an obvious duplicate. It sends back a dependency from a higher-priority goal's task onto a lower-priority goal's task that only avoids a conflict, or one with no kind and reason recorded. A planner does not re-wire, park or draft other tasks, and never bends dependencies to hurry one. Give a task its own priority only when the request has none and the person's words say it goes first or can wait; never for an AI-made task. A content prerequisite in a lower-priority goal is yours to solve by reordering, moving the task into the goal or splitting it; only when none works, ask a `planner_question` (`--because scope`): "bring the lower goal's task forward with its priority, or does the higher goal wait?"

**A revise.** The reasons reach the proposal's planner and its drafts are `draft` again. Fix what they point at with `edit`, `dependency`, `add` or `cancel`, `lint --proposal N`, then `submit --proposal N`; when the plan is dropped, `proposal withdraw N`. A replacement plan's proposal (submitted with `--replacement`) is fixed as a whole and submitted again with `submit --proposal N --replacement <plan.json>`, keeping every acceptance item of the parked task mapped; never edit the parked task. A fix the reasons name is made as they say, without asking. A fix that changes the plan's intent (acceptance, scope, the relation to the goal) goes up only when the queue, the repository, the decision records and the person's precedents cannot settle it: `ask --task ID --kind planner_question --because scope` with everything the person needs and your recommendation, after saving every fix the answer does not touch ("Before stopping at a question"). The answer comes as `answer to ask <id>: ...`; apply it.

A ready task plan review must change comes back `submitted` in a proposal of its own: fix it and `submit --proposal N`. A person's concern goes to the inbox, never to a planner; its `send_back` returns as a revise.

**A runtime planner for a draft** first runs `related ID` (and `search`), judges a follow_up's membership ("A follow_up's membership"), then adopts (`edit`, `lint`, `submit`), drops (`cancel` with its reason and a `note`), or asks (`planner_question` with `adopt` / `cancel` / `keep_draft` and its recommendation) only for what the Basic policy raises, or for a follow_up past the follow_up limit (the prompt says when it applies), which always waits for the person's adopt. `set-goal` and `judge-follow-up` are never adoption. Record why in a `note` or `context`, report briefly and stop. A kept draft waits for a planning request that names it or its revisit time ("A draft's revisit time").

**A recurring draft** comes from the repository's `[[recurring]]` schedule: its interval says only that the need is to be checked now. Check it (for a toolchain, the latest official stable against the version the repository pins), then adopt it made concrete (the exact change, `--verify`, `--paths`, `--evidence`, what you checked in `context`, an open goal that needs it) or cancel it (`not_worth`, or `stale` when already done) with a `note` of what you checked. Neither the interval nor nothing new is a reason to ask.

**Before stopping at a question (a runtime planner).** Decide first everything the answer does not touch (the other drafts of a bundle, the fixes of a revise). Save draft edits with `edit`, and record with `note --task ID` (or `--goal ID`) what you decided, what is left open and why, and what the answer will decide. Then ask and stop: the planner the answer opens goes on from your notes and drafts.

**A draft's revisit time.** When a draft cannot be decided yet but you know when it can, keep it with a time: `"$DAGQ" revisit ID --at <RFC 3339 time> --note '<what to look at then>'`. A runtime planner sets it as it keeps the draft; the person and the inbox set, change or clear it too. A planner takes the draft up again then, once; one that keeps it again sets a new time.

**A follow_up's membership**. A follow_up draft with a source goal (`show ID`) is judged against that source goal's acceptance, not its current goal's, before you adopt or ask. Membership, adoption and priority are three separate calls:

1. **Start from the worker's proposal** (in the material, if any), not a judgement. Read the source goal's acceptance, the source receipt, and earlier judgements (one that still holds is not recorded again).
2. **Ask one question:** could the source goal's acceptance be said to be met without this follow-up? No: `required` (it stays in, or goes back to, the source goal). Yes: `out_of_scope` (it goes to another goal). Size, priority, relatedness and the worker's `category` do not decide it. When you cannot tell, look for grounds and decide; ask (`planner_question --because scope`) only when the goal's intent is needed. `undecided` records why it could not be settled, not a place to park it.
3. **Record it before you adopt or ask:** `"$DAGQ" judge-follow-up ID` with the classification, the acceptance item, the reason and the evidence (`judge-follow-up --help`). `out_of_scope` names the destination goal and, when it is above `low`, the item of its acceptance that needs it.
4. **The destination of `out_of_scope`:** first `search` and `goal list` for an open goal whose problem it belongs to. A candidate above the same tag's deferral goal's priority must have an acceptance item that needs it; when you cannot name one, it goes to the **deferral goal** of the source goal's main tag: one `low` goal per tag holding the theme's improvements no acceptance needs (`goal list --tag T`, else `goal add --draft --priority low --tag T`). Never a leftover goal per source goal, never an unrelated large maintenance goal. A goal without tags: create a fitting goal only when none fits.
5. **Then decide adoption.** Adopt it, drop it (`cancel` with its reason and a `note` giving the grounds), or ask per the Basic policy. Priority follows "Priority" above, not membership.
6. **Correcting it.** Between `required` and `out_of_scope` record a correction of the latest judgement with a reason naming what was wrong; never back to `undecided`. After an achieved close: `reference/goal-close.md`, "After an achieved close".
7. **Acceptance is the person's.** Never weaken a goal's acceptance to put a follow-up out of scope: ask (`planner_question --because scope`). Once a goal's acceptance changes, read each judgement made against the older version against the new text and record it again before the goal can close or the draft be submitted.

**Deferring what an acceptance does not need**. Apply the same question to a goal's remaining tasks when a goal (above all a `high` or higher one) holds work its acceptance does not need: an improvement it does not need leaves for a fitting goal or the tag's deferral goal (step 4). A follow-up moves by its judgement (step 3; a judged one is never moved elsewhere with `set-goal`: record a correction, step 6); any other task with `set-goal`, its grounds in the proposal or its `context`. Only a `draft` or `ready` task moves; clear its own priority (`set-priority TASK --inherit`) or say why it keeps one. A person's priority stays: leave it, or ask. One found required after an achieved close takes the correction path (`reference/goal-close.md`, "After an achieved close").

A goal gap draft is required by birth: do not move it out of its goal. Plan review checks the item, the reason and the evidence, so write the mapping someone can check without redoing your investigation.

**A runtime planner for a request** reads the person's words, the inbox's note, what they refer to and the goals they lead to, looks for work that already covers them, and does one: plans it, declines it (`request decline N --reason '<why>'`: done already, a duplicate in flight, or not plannable as asked), or asks (`ask --request N --kind planner_question` with `plan` / `decline` and its recommendation) only for what the Basic policy raises. What it plans is human-made. For a `replan request`, it continues the parked run (`request decline N --continue-run --reason '<diagnosis>'`) or submits child tasks with `submit ... --replacement <plan.json>`; it never edits the parked task.

To plan it (the `dagq-planner` skill's section 1): a goal (`goal add --draft`) unless it is a one-shot task or fits an open goal, and tasks with `add --goal` per "Task fields", `--context` beginning `from request N`. After each `add`, `related ID`. Check the order with `graph --goal ID`, then `lint` and `submit`, and report the proposal ID. The worker's provider: no flag unless the person's words ask for one (`reference/provider.md`, "Which to choose").

**A runtime planner for a finding** checks `search` / `related` first, then submits tasks for it (`submit ... --finding N`) on an open goal whose acceptance needs them, a new draft goal or no goal (an improvement only in a goal of `normal` or lower or none), dismisses it (`finding dismiss N --reason`), or asks only what a person must decide (`reference/observer.md`).

**A `ci_failure` finding** says the watched branch's CI turned red with new failures, naming the failing tests and the commit range. Copy them into the fix task's description; the task takes no priority of its own. When an open task already fixes the same tests (`search`, `related`, `ci failures`), add none: `finding dismiss N --covered-by TASK --reason '<why>'`.
