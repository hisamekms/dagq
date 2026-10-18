# Inspect commands and how to use them

Read this to pick the command that answers a question about the queue, to find a duplicate or work already done before submitting, to decide what runs first and how priority orders claiming, to make a task wait for another goal, or to edit, move or withdraw planned work.

Each command's flags and the fields it prints are in `"$DAGQ" <command> --help` (`"$DAGQ" goal show --help` for a subcommand) and in its output itself; this file does not list them. Every command here reads only unless it says otherwise.

| Command | Answers |
| --- | --- |
| `goal list` / `goal show ID` | Every goal, unclosed first, each by priority, with its task counts / one goal, its tasks, latest events and notes, and the unfinished tasks of any goal waiting on it (`dependents`) |
| `list` / `show ID` | Unfinished tasks, a page at a time / one task with its dependencies, latest run, events, notes and asks |
| `search QUERY` / `related TASK` | Tasks, goals, notes and landed commits matching words / the tasks most related to one by fixed clues (below) |
| `graph` / `candidates` | Dependencies, what each task still waits for and the critical chain / which ready tasks the supervisor claims next, which it deferred and why, and what holds every claim (below) |
| `events` / `timeline RUN` | Events oldest first, filtered by kind, task, run or goal / one run's events and the gaps between them with their reason |
| `notes` / `note` | Notes on a goal, task or run / record one (changes the queue: an `observation` event) |
| `findings` / `finding dismiss` / `finding resolve` | The observer's findings and how to decide them: `reference/observer.md`. `--covered-by TASK` only for a `ci_failure` finding and an open task that already fixes its tests (`reference/register.md`) |
| `ci failures [--task ID]` | The tests already failing on the branch the supervisor's CI watch watches; `--task ID` sets apart those the task fixes. Workers and jobs may not run it |
| `status` / `doctor` | Supervisors, slots, waiting runs and the runs in flight / the same per run with what blocks its recovery (`reference/doctor.md` of the recover skill for acting on it) |
| `stats` | Where time goes, from run events: per run, goal and overall, with alerts, holds, deferrals, sessions and review reasons |
| `kpi` / `forecast` / `marks` / `mark` / `report` | KPIs, forecasts and change marks: `reference/kpi.md` |
| `observe --history` / `observe --input` | The observer's past observations and their whole input |
| `locate` | The queue this directory resolves to, without opening it (`reference/locate.md`) |

## Read the output

- **Pages.** `list` prints `{"tasks", "next", "total"}`. A null `next` means the last page; otherwise pass it as `--before NEXT` with the same filters. Decide whether more pages exist from `next` alone, never by counting `tasks`. `events` and `notes` page with their `cursor` the same way. In `stats`, `next_cursor` pages only the runs and what derives from them; its period aggregates count the whole window, so never sum them across pages.
- **Cut texts.** `show`, `goal show` and `doctor` may cut long texts, ending them in `…` and marking the object `truncated: true`. Add `--full` only when you need the whole text, every run, every event payload (a receipt), every ask or a run's `run_dir`. Take the text to edit from `show --full`, never from the cut one.
- **One task.** Prefer `show ID` to `list --full`. A `claim_deferral` key in the compact `show` means the supervisor is passing over the task at claim now and says why; `show --full` leaves it out (read the `claim_deferred` events instead).

## Statuses

Task: `draft` → `submitted` (in a proposal, waiting for plan review; never claimed) → `ready` → `in_progress` → `completed`, or `canceled`. Only plan review, a person's explicit `ready --bypass-review` and a retry of an `in_progress` task whose runs failed make a task `ready`. A task stays `in_progress` while any run is unfinished or awaiting integration.

Run: `claimed`, `starting`, `running` and `validating` are unfinished. `awaiting_integration` passed validation and waits for `integrate`, which runs the verification commands after its rebase; `integrating` is being landed now. `needs_session` hit a rebase conflict, a failed verification or a scope violation, and the supervisor resumes its session a limited number of times, then hands it to the recovery job (`last_error` says why). `integrated` landed (`result_commit`) and completed the task. `failed` and `interrupted` keep their worktree for inspection and go to the recovery job. A dependent task waits until every predecessor is `completed`.

A run whose live session only waits for a person's answer leaves its slot (`status`'s `waiting`) and keeps its lease, session and status; the answer returns it to a slot before new claims. The inbox skill's `reference/status.md` says how to read it.

## Claude session time

`stats` records each Claude session the runtime uses as a span per kind (`worker`, `resume`, `revise`, `review`, `triage` for the recovery job, `observer`, `plan_review`, `goal_review`, `throughput_review`, and through the plugin's hooks `inbox`, `planner`, `runtime_planner`) with two times: `open` (wall clock from open to close) and `active` (time spent on turns). `open` much longer than `active` means the session sat idle, waiting for a review, a person or its end.

- `sessions.by_kind` covers the window (`--since` / `--until`): the inbox and the planners have no run and appear only there. `active_ratio` is active over open; `open_now` counts spans still open at the window's end, `inferred` spans closed without a seen end, `active_unavailable` spans whose transcript could not be read (yet). `sessions.by_route` splits the same by `interactive` / `headless`.
- `execution_tokens` counts the tokens of each turn, job and session cut that ended in the window; `recorded_from` says when the records begin and `coverage` (`full` / `partial` / `none`) how much of the window they cover. Tokens are advisory: never for billing or authorization.
- A context or token value not recorded is counted as missing, never as 0.

## Find duplicates and work already done (`search`, `related`, `--duplicate-of`)

Take candidates from `search` and `related`, read only those in full (`show ID --full`), and judge only them:

1. **Before `add`**: `search` the problem's words: file names, test names, decision record numbers and the title you are about to write. Look at hits in every status: a `completed` task or a landed `commit` means the work may be done; an unfinished task may be the same work. Narrow with `--status` or `--kind` when the hits are many.
2. **After `add`, before `submit`**: `related ID` on each new draft. Its clues come from the task as registered (declared `--paths`, the texts, the goal), so it cannot run before `add`. Read the top few candidates' `clues`, then only their full text.
3. **A duplicate or work already done**: do not submit it; `cancel ID --duplicate-of X`, where X is the task it duplicates (a `completed` X means "already done by X"). X must exist, differ from ID and not be `canceled`; if X was itself canceled as a duplicate, the error names its target: use that. A plain `cancel` leaves no link, so use `--duplicate-of` whenever the reason is a duplicate. If the new task only overlaps X, keep it and narrow it (`edit`), or depend on X (`dependency add`).

Every planner does this, a runtime planner for a follow_up or finding draft included. `search` and `related` change nothing, so any role may run them. Plan review checks the same; it does not replace your own check.

## Wait for another goal (goal dependencies)

A task can depend on a goal as well as on tasks. It is not claimed until that goal is closed `achieved`; whether the goal's tasks are all finished does not matter, since follow-up and gap drafts can join the goal until it closes (`reference/goal-close.md`).

```sh
"$DAGQ" add "TITLE" --goal 2 --depends-on-goal 1 ...   # repeatable; combines with --depends-on
"$DAGQ" dependency add TASK --goal 1                    # or remove; instead of a PREDECESSOR
```

- **When to use which.** To wait for another goal's outcome, depend on the goal, not on its last task: a task dependency is met once that task is `completed`, even while drafts added to the goal later are unfinished. Order inside one goal stays task dependencies.
- **Rules.** Only a `draft`, `submitted` or `ready` task gains or loses one. A dependency on the task's own goal and any cycle through task dependencies, goal dependencies and goal membership are refused; the error names which.
- **Abandoned.** A goal closed `abandoned` never releases its dependents: they wait until a planner removes the dependency (`dependency remove TASK --goal ID`), points it elsewhere or cancels the task, as the person's request says (`reference/goal-close.md`).

## Decide what to run first with `graph`

When planning a goal or deciding what to submit next, read `"$DAGQ" graph --goal ID` (or `graph`) instead of each task's dependencies:

1. `critical` is the chain that holds back the most work. Its first task should be `ready` and running; if it is `draft`, complete it and `submit` it before tasks with small `unblocks`.
2. A task with a large `unblocks` missing from `candidates` waits on its `ready_after`: finish (or review and land) those first. A ready task with an empty `ready_after` missing from `candidates` is in `deferred`, with the supervisor's reason.
3. When one task blocks many, split it or remove a dependency that is not real (`dependency remove`) so more run in parallel.
4. `candidates` is the order the supervisor claims in, as of its last judgment of the deferrals; what holds every claim is in `held`. There is no need to register or submit in a particular order: to put a task ahead, give it a priority (below).

The deferrals are judged only in a pass that may claim, so while the slots are full or a hold is on, an earlier deferral stays: read its `since` and `held`. `graph --format d2|svg` draws the near-term diagram; SVG needs `d2` and its TALA plugin on PATH (`doctor` shows where they resolve).

## Priority and claim order

A goal and every task have one of five priorities. Pick it by what waiting costs, not by how much the work matters in general:

| Level | Use for | Example |
| --- | --- | --- |
| `interrupt` | An exceptional cut-in that must run before every other ready task. Not for routine use: when many tasks are `interrupt`, it orders nothing | A goal the person decided to put ahead of the whole queue |
| `urgent` | A defect that is stopping operation now | The supervisor or landing is broken |
| `high` | Work to do early, because other work builds on it | A cleanup or refactor several planned tasks assume |
| `normal` | Everything else | |
| `low` | Work that can wait | A conditional proposal, a remedy for a minor finding |

```sh
"$DAGQ" goal add "TITLE" --priority high ...       # the goal's; names only (no numbers)
"$DAGQ" goal edit GOAL --priority low               # a draft or open goal
"$DAGQ" add "TITLE" --goal 1 --priority high ...   # the task's own; omit it to inherit the goal's
"$DAGQ" set-priority TASK urgent                    # the task's own
"$DAGQ" set-priority TASK --inherit                 # clear it: inherit the goal's again
```

- **Where a task's priority comes from.** The goal's priority is the source: a task without its own takes its goal's; `priority_source` says which. Give a task its own only to raise or lower a few tasks of a goal; it does not follow the goal's changes. What a task moved to another goal keeps: `reference/register.md`.
- **Claim order.** Highest effective priority → most `unblocks` → lowest ID. Goals have no rank: order goals by their priority.
- **Execution classes.** Within one priority, when both classes (implementation and Spike) have ready tasks, the class whose task has waited past the class aging threshold, and longer than any of the other class's, goes first. Aging never moves a task above a higher priority, never passes the Spike limit or a deferral, and never touches `interrupt` tasks. To hurry a Spike, raise its priority (not to `interrupt`).
- **Slot order.** A free slot goes to one line of resumes of `needs_session` runs, recovery jobs of `failed` or `interrupted` runs, and claims, sorted by each one's task's effective priority; on the same priority a resume comes first, then a recovery job, then a claim. So a `high` ready task is claimed before a `normal` task's resume. A held kind only leaves the line; it keeps no slot free.
- **Effective priority.** A task's priority raised to the highest of the `ready` tasks (outside a draft goal) that wait on it, directly or transitively. A `high` ready task pulls its unfinished predecessors ahead: there is no need to raise them one by one. A draft set aside pushes nothing forward.
- **When it can change.** A task's own while it is `draft`, `submitted` or `ready`; while `in_progress` only by the user and the inbox (a planner changes only tasks that have not started); never on a finished task. A goal's on a draft or open goal. A change takes effect at the next claim, resume or recovery job and never stops a running run.
- **Putting parked work behind or ahead.** When many runs wait in `needs_session` and other work should go first, `set-priority TASK low` on the parked tasks (or `high` on the one to resume first); `--inherit` gives the task back to its goal. Canceling is the other way out for a parked run nobody needs.
- **Express urgency with priority only.** Do not move other tasks back to `draft`, and do not add or remove dependencies that are not real, to get one task claimed first.
- **No aging of priority.** Nothing raises a priority over time: a `low` task, its resume and recovery jobs wait as long as higher ones keep arriving. Check `graph` for ready tasks, and `list --status in_progress` for parked runs, that never reach the front, and raise them by hand.

## Edit a draft or submitted task

Fix a draft or submitted task in place instead of canceling it and registering it again (that changes its ID and its dependents' edges):

```sh
"$DAGQ" edit TASK --description "..." --acceptance "..."   # also --title, --context
"$DAGQ" edit TASK --verify 'CHECK' --verify 'TEST'
"$DAGQ" edit TASK --evidence e2e --paths 'src/**'          # --no-verify / --no-evidence / --no-paths empty a list
"$DAGQ" edit TASK --change fix                              # one of the repository's changes (reference/scope.md)
```

- Each given field replaces the old value; a repeatable flag replaces the whole list, so pass every value it should keep. The values follow the `add` rules.
- **When it can change.** A `draft` or `submitted` task. A plan review already running may pass the old text, so fix a proposal while it is sent back (`draft`) rather than while it is `submitted`. A `ready` task changes only through plan review or by taking it back with `draft ID`, editing it and submitting it again. On an `in_progress` task the user or the inbox may change only `--verify` / `--no-verify`, once its latest run ended and no live run remains; a live run keeps its verification commands. A finished task is refused.
- Priority, goal and dependencies have their own commands: `set-priority`, `set-goal`, `dependency add|remove`.

## Withdraw a proposal (`proposal withdraw`)

```sh
"$DAGQ" proposal withdraw ID
```

Its planner (or a person) withdraws a `submitted` or `revising` proposal without plan review, in two cases:

- **A revising proposal that cannot be resubmitted.** Its drafts were all readied with `ready --bypass-review` or canceled, so `submit --proposal ID` fails with `EmptyProposal` and the proposal would stay `revising` for good.
- **The plan is dropped or reshaped.** The person no longer wants it, or its tasks go into another proposal.

The proposal ends `canceled`, its `submitted` tasks go back to `draft` (others stay as they are), and an open `approve_plan` ask on it is closed, so no answer lands on the tasks' next proposal. Then submit the drafts in another proposal, `cancel` them, or leave them; a draft with an origin (`follow_up`, `goal_gap`) goes back to the runtime's planners. An `accepted` or `canceled` proposal is refused; the observer and the review jobs cannot run it.

## A run that needs paths outside its task

A task registered with `--paths` may change only what they match (`reference/scope.md`). Two signs say a run needed more:

- **`scope_violation`.** Validation parked the run as `needs_session` or `integrate` deferred it; `events --run RUN --kind scope_violation --full` names the paths. The supervisor resumes the session to restore them; nothing is needed while the task's paths are right.
- **A `failed` receipt naming paths.** The worker found the task truly needs a path outside its globs and stopped; its `summary` names the path and the work.

Next: while the task is `draft`, `submitted` or `ready`, widen its globs with `set-paths TASK --paths ...` (every glob it should keep) or remove the limit with `--none`. A claimed run keeps the paths it started with, so after a failed run register the task again with wider `--paths` and the verification those paths need (per the repository's rules: `reference/scope.md`).

## Change a goal or a task's goal

```sh
"$DAGQ" set-goal TASK GOAL                # move a draft, submitted or ready task into an open goal
"$DAGQ" set-goal TASK --none              # take a draft, submitted or ready task out of its goal
"$DAGQ" goal edit ID --constraints "..."  # replace one or more fields; see goal edit --help
"$DAGQ" goal close ID --verdict achieved  # or abandoned; see goal-close.md
"$DAGQ" goal review ID                    # rearm the goal review after a review by hand or a keep_open answer (person or inbox)
```

A closed goal accepts no task. Take an `in_progress` task back with `draft ID` only if its run is finished; a `completed` task keeps the goal it landed with. A run already claimed keeps the prompt it started with; runs claimed after a `goal edit` see the new text. `goal edit --tag` replaces every tag, from the repository's configured tags when it lists them. A closed goal's tags and texts can still change, its priority cannot.
