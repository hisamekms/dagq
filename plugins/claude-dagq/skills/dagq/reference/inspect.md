# Inspect commands and how to use them

Read this to pick the command that answers a question about the queue, to find a duplicate or work already done before submitting, to decide what runs first and how priority orders claiming, to make a task wait for another goal, or to edit, move or withdraw planned work.

Each command's flags and the fields it prints are in `"$DAGQ" <command> --help` (`"$DAGQ" goal show --help` for a subcommand) and in its output itself; this file does not list them. Every command here reads only unless it says otherwise.

| Command | Answers |
| --- | --- |
| `goal list` / `goal show ID` | Which goals there are and how far each is / one goal and what waits on it |
| `list` / `show ID` | Unfinished tasks, a page at a time / one task |
| `search QUERY` / `related TASK` | Tasks, goals, notes and landed commits matching words / the tasks most related to one (below) |
| `graph` / `candidates` | Dependencies, what each task waits for and the critical chain / which ready tasks the supervisor claims next and why the others wait (below) |
| `events` / `timeline RUN` | What happened, by kind, task, run or goal / where one run's time went |
| `notes` / `note` | Notes on a goal, task or run / record one (changes the queue) |
| `findings` / `finding dismiss` / `finding resolve` | The observer's findings and how to decide them: `reference/observer.md`. `--covered-by TASK` only for a `ci_failure` finding (`reference/register.md`) |
| `ci failures [--task ID]` | The tests already failing on the watched branch; `--task ID` sets apart those the task fixes |
| `status` / `doctor` | Supervisors, slots, waiting runs and the runs in flight / the same per run with what blocks its recovery (the recover skill's `reference/doctor.md`) |
| `stats` | Where time goes, per run, goal and overall. Stopped in the current build (`reference/kpi.md`): read `timeline RUN` and `events` instead |
| `kpi` / `forecast` / `marks` / `mark` / `report` | KPIs, forecasts and change marks: `reference/kpi.md` |
| `observe --history` / `observe --input` | The observer's past observations and their whole input |
| `locate` | The queue this directory resolves to, without opening it (`reference/locate.md`) |

## Read the output

- **Pages.** Page `list`, `events`, `notes` and `stats` with the cursor the output gives, with the same filters, until it gives none; never decide by counting items. `stats`' totals already cover the whole window: never sum them across pages.
- **Cut texts.** `show`, `goal show` and `doctor` may cut long texts, ending them in `…`. Take the text to edit from `show --full`, never from the cut one.
- **Holds.** Holding and releasing: the `dagq-recover` skill's `reference/hold.md`.
- **One task.** Prefer `show ID` to `list --full`. To see why a ready task is passed over at claim, read `show ID` or `candidates`.

## Statuses

Task: `draft` → `submitted` (waiting for plan review) → `ready` → `in_progress` → `completed`, or `canceled`. A dependent task waits until every predecessor is `completed`. A hold is a mark beside the status, not a status. What a run's state asks of you: the inbox skill's `reference/status.md`.

## Claude session time

Read a Claude session's time and tokens with `"$DAGQ" stats` (`stats --help`); while `stats` is stopped (`reference/kpi.md`), read the `session_opened` and `session_closed` events instead.

- A session open much longer than it was active sat idle, waiting for a review, a person or its end: look for what it waited on, not for slow work.
- The inbox and the planners have no run: read them in the window's sessions, not per run.
- Tokens are advisory: never for billing or authorization.
- A value not recorded is missing, never 0.

## Find duplicates and work already done (`search`, `related`, `--duplicate-of`)

Take candidates from `search` and `related`, read only those in full (`show ID --full`), and judge only them:

1. **Before `add`**: `search` the problem's words: file names, test names, decision record numbers and the title you are about to write. Look at hits in every status: a `completed` task or a landed `commit` means the work may be done; an unfinished task may be the same work.
2. **After `add`, before `submit`**: `related ID` on each new draft (it needs the registered task). Read the top few candidates' clues, then only their full text.
3. **A duplicate or work already done**: do not submit it; `cancel ID --duplicate-of X` (`reference/register.md`, "Cancel reasons"). If the new task only overlaps X, keep it and narrow it (`edit`), or depend on X (`dependency add`).

Every planner does this, a runtime planner for a follow_up or finding draft included. Plan review checks the same; it does not replace your own check.

## Wait for another goal (goal dependencies)

A task can depend on a goal as well as on tasks: it is not claimed until that goal is closed `achieved` (`reference/goal-close.md`).

```sh
"$DAGQ" add "TITLE" --goal 2 --depends-on-goal 1 ...   # repeatable; combines with --depends-on
"$DAGQ" dependency add TASK --goal 1                    # or remove; instead of a PREDECESSOR
```

- **When to use which.** To wait for another goal's outcome, depend on the goal, not on its last task: follow-up and gap drafts can join the goal after that task completes. Order inside one goal stays task dependencies.
- **Abandoned.** A goal closed `abandoned` never releases its dependents: a planner removes the dependency, points it elsewhere or cancels the task, as the person's request says.

## Decide what to run first with `graph`

When planning a goal or deciding what to submit next, read `"$DAGQ" graph --goal ID` (or `graph`) instead of each task's dependencies:

1. Start from the chain the output names as critical. Its first task should be `ready` and running; if it is `draft`, complete it and `submit` it before tasks that unblock little.
2. A task that unblocks much but is missing from `candidates` waits on what the output names: finish (or review and land) those first. Read why each other ready task waits in `candidates`.
3. When one task blocks many, split it or remove a dependency that is not real (`dependency remove`) so more run in parallel.
4. There is no need to register or submit in a particular order: to put a task ahead, give it a priority (below).

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
"$DAGQ" goal edit GOAL --priority low               # the goal's
"$DAGQ" set-priority TASK urgent                    # the task's own
"$DAGQ" set-priority TASK --inherit                 # clear it: inherit the goal's again
```

- **Where a task's priority comes from.** The goal's: give a task its own only to raise or lower a few tasks of a goal (`reference/register.md`, "Priority").
- **Claim order.** A higher effective priority is claimed first; within one priority, what unblocks more. Goals have no rank: order goals by their priority. To hurry a Spike, raise its priority (not to `interrupt`); a Spike the repository's caps hold back waits as `ready`: do not ready it another way (`reference/spike.md`).
- **Slot order.** Resumes, recovery jobs and claims compete for a free slot by their task's effective priority, so a `high` ready task is claimed before a `normal` task's resume.
- **Effective priority.** A `high` ready task pulls its unfinished predecessors ahead: there is no need to raise them one by one.
- **Putting parked work behind or ahead.** When many runs wait in `needs_session` and other work should go first, `set-priority TASK low` on the parked tasks (or `high` on the one to resume first). Canceling is the other way out for a parked run nobody needs.
- **Express urgency with priority only.** Do not move other tasks back to `draft`, and do not add or remove dependencies that are not real, to get one task claimed first.
- **No aging of priority.** Nothing raises a priority over time: check `graph` and `list --status in_progress` for work that never reaches the front, and raise it by hand.

## Edit a draft or submitted task

Fix a draft or submitted task in place instead of canceling it and registering it again (that changes its ID and its dependents' edges): `"$DAGQ" edit TASK ...` (`edit --help`). Each given field replaces the old value; a repeatable flag replaces the whole list, so pass every value it should keep. A blank title, description or acceptance is refused; to drop a draft, `cancel` it.

- Fix a proposal while it is sent back (`draft`) rather than while it is `submitted`: a review already running may pass the old text. A `ready` task changes only through plan review or by taking it back with `draft ID`.
- Priority, goal and dependencies have their own commands: `set-priority`, `set-goal`, `dependency add|remove`.

## Withdraw a proposal (`proposal withdraw`)

Its planner (or a person) withdraws a proposal with `"$DAGQ" proposal withdraw ID` when the plan is dropped or reshaped, or when a sent-back proposal cannot be resubmitted (`EmptyProposal`: its drafts were all readied by hand or canceled). Its `submitted` tasks go back to `draft` (tasks in other states stay as they are): submit them in another proposal, `cancel` them, or leave them.

## A run that needs paths outside its task

A task registered with `--paths` may change only what they match (`reference/scope.md`). A `scope_violation` (`events --run RUN --kind scope_violation --full` names the paths) needs nothing while the task's paths are right: the supervisor has the session restore them. A `failed` receipt naming paths says the task truly needs more: while the task is `draft`, `submitted` or `ready`, widen its globs with `set-paths`; after a run, register the task again with wider `--paths` and the verification those paths need.

## Change a goal or a task's goal

```sh
"$DAGQ" set-goal TASK GOAL                # move a draft, submitted or ready task into an open goal
"$DAGQ" set-goal TASK --none              # take it out of its goal
"$DAGQ" goal edit ID --constraints "..."  # see goal edit --help
"$DAGQ" goal close ID --verdict achieved  # or abandoned; see goal-close.md
"$DAGQ" goal review ID                    # rearm the goal review after a review by hand or a keep_open answer (person or inbox)
```

A run already claimed keeps the prompt it started with; runs claimed after a `goal edit` see the new text.
