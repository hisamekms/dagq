# Closing a goal

A goal is closed once. Closing it after its last task is not a planner's step (ADR-0047 decisions 16 and 43): the supervisor's headless **goal review** job judges it.

## The goal review job

The supervisor starts it (one at a time, needing no run slot) for an `open` goal whose tasks are all `completed` or `canceled`, with at least one `completed` and no `draft` or `submitted` task left (a follow_up draft from a receipt still waiting for its runtime planner, a `keep_draft`, an open `planner_question` hold it back), no unsettled follow-up (below), no `approve_goal` ask of it open, and its input changed since its last review (its tasks' statuses, its acceptance version, a follow-up registered from it or a membership judgement recorded). The job reads the goal, each task's integrated receipt (`summary`, evidence, `follow_ups`), the goal's notes and earlier reviews, and the repository, and returns one verdict, which the runtime applies after checking the goal, its tasks, its acceptance and its follow-ups' judgements have not changed meanwhile (else the verdict is dropped and the review taken again once settled):

- **achieved**: the runtime closes the goal `achieved` (`goal_closed` with `by: "goal_review"`); `goal_review_finished` has `criteria`, the grounds per acceptance criterion. Its `dependents` are released.
- **gaps**: each gap becomes a `draft` task on the goal (context `goal_gap: goal review N of goal ID found this missing`; `draft_origins` origin `goal_gap`), which a runtime planner adopts, drops or asks about like a follow_up draft (`reference/register.md`, "A runtime planner for a draft"). The goal stays open; once those drafts finish, the review starts again. A fourth `gaps` in a row turns into an ask.
- **ask**: only when a person is needed (the acceptance changes, abandoning or splitting it, or the job cannot judge it): an `approve_goal` ask in the inbox, on the goal's first task, with `reason_category` `scope` or `discard`. Options: `achieved`, `abandoned`, `gaps` (or `gaps: <what is missing>`), `keep_open`, plus the job's own. The supervisor applies the first four (`goal_decided`): it closes the goal, registers the gaps as drafts, or leaves it open until its input changes (its tasks, its acceptance or its follow-ups' judgements). An answer it does not apply (the job's own option, free text) comes back to the inbox to carry out and `ask close`.

A failed job (non-zero exit, timeout, no verdict) leaves the goal open and is not tried again by itself until its input changes as above: the inbox shows `goal review by hand` (`goal_review_failed`, on the goal's first task). On the person's word, `goal review ID` starts it again (also after a `keep_open` answer, once there is a reason to look again):

```sh
"$DAGQ" goal review ID   # rearm the goal review; the person or the inbox, never a planner
```

Follow it with `goal show ID` and `events --goal ID` (`goal_review_started`, `goal_review_finished`, `goal_review_failed`, `goal_review_rearmed`, `goal_decided`, and after an achieved close `goal_correction_decided` / `goal_reopened` for a `correct_goal` ask).

## What `goal close` is still for

A planner, the person or the inbox (on the person's word) still runs `goal close`, but not as the routine end of a goal:

- **A draft goal not adopted** (`goal add --draft`, or an old observer's): `goal close ID --verdict abandoned`.
- **The person's decision**, after `goal review by hand` or in a request to the inbox (`request add --ref goal:N`, whose runtime planner carries it out): closing the goal `abandoned`, or `achieved` without another review. The person may also type it in their own terminal (no `DAGQ_ROLE`).

```sh
"$DAGQ" goal close ID --verdict achieved   # or abandoned
```

`achieved` is refused while any task is `draft`, `submitted`, `ready` or `in_progress` (the error names the count and status); cancel or finish them first. It is also refused while a follow-up registered from the goal's tasks (wherever it belongs now) is neither `completed` nor `canceled` and is not judged, is `undecided`, was judged before the goal's acceptance last changed, or is `required` but outside the goal (`goal <ID> cannot be closed as achieved: its follow-up(s) <task> <why>, ...`): its membership is judged with `judge-follow-up` (`reference/register.md`, "A runtime planner for a draft"). A follow-up judged `out_of_scope` does not hold the goal, finished or not. The same condition holds for the goal review and an `approve_goal` answer `achieved`. Closing it `achieved` releases the unfinished tasks that depend on the goal (`dependents` in `goal show ID`; `reference/inspect.md`, "Wait for another goal"), so a `ready` one may be claimed right after the close: tell the person which ones will start. `abandoned` records that the goal is given up: it is refused while a task is `in_progress`, and it does not cancel the goal's `draft`, `submitted` or `ready` tasks, so cancel them first or the supervisor still runs them. It never releases the `dependents`: they wait for good. Before closing `abandoned`, decide each of them (as the person's word or a request says): remove the dependency (`dependency remove TASK --goal ID`), make it depend on another goal or task, or cancel it; `goal show ID` after the close still lists the ones left. Both verdicts are final; further work on the same problem is a new goal. A closed goal refuses new tasks (a follow-up of a task of a closed goal is registered without a goal). `goal show ID` afterwards has `closed: true` at the top level, `verdict` and `closed_at` inside `goal`, and a `goal_closed` event with the task counts at close time.

Report a goal to the person as: its title and verdict (or open), its task counts from `goal list`, which tasks are `in_progress` or blocked, and, once every task is finished, whether its goal review ran and what it decided (closed, gaps registered as drafts, an `approve_goal` ask, or failed).
