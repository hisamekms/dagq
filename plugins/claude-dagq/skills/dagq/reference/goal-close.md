# Closing a goal

A goal is closed once. Closing it after its last task is not a planner's step: the supervisor's headless **goal review** job judges it.

## The goal review job

The supervisor starts it for an open goal whose tasks are all `completed` or `canceled` (at least one `completed`, no `draft` or `submitted` left), whose follow-ups are settled ("When a goal may close achieved" below), and whose input changed since its last review. It returns one verdict:

- **achieved**: the goal closes `achieved` and its `dependents` are released.
- **gaps**: each gap becomes a `draft` task on the goal, which a runtime planner adopts, drops or asks about like a follow_up draft (`reference/register.md`, "A runtime planner for a draft"). The review starts again once they finish; gaps found again and again turn into an ask.
- **ask**: only when a person is needed (the acceptance changes, abandoning or splitting it, or the job cannot judge): an `approve_goal` ask in the inbox. An answer the supervisor does not apply itself comes back to the inbox to carry out and `ask close`.

A goal that waits only for an unsettled follow-up shows in `status`'s attention as `request a plan for the follow-ups of goal G`. A planner's `judge-follow-up` clears it; the runtime never judges or moves a follow-up itself.

A failed job leaves the goal open (the inbox shows `goal review by hand`). On the person's word, `goal review ID` starts it again (also after a `keep_open` answer, once there is a reason to look again):

```sh
"$DAGQ" goal review ID   # rearm the goal review; the person or the inbox, never a planner
```

Follow it with `goal show ID` and `events --goal ID`.

## What `goal close` is still for

A planner, the person or the inbox (on the person's word) runs `goal close` only for:

- **A draft goal not adopted**: `goal close ID --verdict abandoned`.
- **The person's decision**, after `goal review by hand` or in a request to the inbox (`request add --ref goal:N`, whose runtime planner carries it out): closing the goal `abandoned`, or `achieved` without another review. The person may also type it in their own terminal (no `DAGQ_ROLE`).

```sh
"$DAGQ" goal close ID --verdict achieved   # or abandoned
```

- `achieved` is refused while a task is unfinished or a follow-up is unsettled (below). Cancel or finish the tasks, and judge the follow-ups with `judge-follow-up` (`reference/register.md`, "A follow_up's membership"); moving one out with `set-goal` without a judgement does not count. Closing it releases the tasks that depend on the goal (`dependents` in `goal show ID`; `reference/inspect.md`, "Wait for another goal"), so a `ready` one may start right after: tell the person which ones will.
- `abandoned` is refused while a task is `in_progress`, and it cancels nothing: cancel the goal's `draft`, `submitted` and `ready` tasks first, or the supervisor still runs them. It never releases the `dependents`: before closing, decide each of them as the person's word says (remove the dependency with `dependency remove TASK --goal ID`, point it at another goal or task, or cancel it).
- A goal closed `abandoned` never opens again; one closed `achieved` opens again only by the answer `reopen` to a `correct_goal` ask (below). Further work on the same problem is a new goal; a closed goal refuses new tasks.

## When a goal may close achieved

The required work is done and every follow-up found from the goal's tasks has its membership judged: every task is `completed` or `canceled`, and each unfinished follow-up has a current judgement (not missing, not `undecided`, not older than the goal's acceptance) and is not `required` while outside the goal. A follow-up judged `out_of_scope` need not be done. `abandoned` claims nothing, so this does not hold it.

## After an achieved close

A misclassification found after the goal closed `achieved` is recorded with the same `judge-follow-up` (a planner, or the person or the inbox on their word); the close and the earlier judgements stay.

- **An ordinary correction is the planner's**: a wrong destination, or `out_of_scope` judged again with better grounds. It moves a draft or ready follow-up to the open destination; no one is asked.
- **A correction that says the acceptance was not met** (a follow-up judged `required` after the close, new or from `out_of_scope` / `undecided`; a `required` recheck is not one) opens a `correct_goal` ask to the person. While it is open, `judge-follow-up` and `set-goal` on that follow-up are refused. The person chooses:
  - `reopen`: the goal opens again, the follow-up moves back into it if it can, and released tasks not yet claimed wait for it again; running ones go on.
  - `correct_verdict`: the goal stays closed with a record that achieved was wrong. A planner puts the fix in a fix goal, an existing fitting goal first (`reference/register.md`, "A follow_up's membership").
  - `keep_achieved`: the acceptance was met after all.

  After `correct_verdict` or `keep_achieved` the judgement stays `required` until a planner records an `out_of_scope` correction naming the follow-up's destination.

How the inbox shows it: `${CLAUDE_PLUGIN_ROOT}/skills/dagq-inbox/reference/asks.md`, `correct_goal`.

Report a goal to the person as: its title and verdict (or open), its task counts from `goal list`, which tasks are `in_progress` or blocked, and, once every task is finished, whether its goal review ran and what it decided (closed, gaps registered as drafts, an `approve_goal` ask, or failed).
