# Goal review by hand

Read this when the attention `goal review by hand` reaches the person (the `dagq-recover` skill, section 8). Act only on the person's word.

A finished goal is judged by the supervisor's headless goal review job, not by a planner (`skills/dagq/reference/goal-close.md`). Attention `goal review by hand`: that job failed and the goal stays open.

The same `next` on the kind `replacement_unresolved`, on a `replaced` task: the record of what replaced it names no task, a task that is not there, or loops. It is never resolved, so the goal starts no review and does not close as `achieved`, and a recurring task's key stays held; the record is not fixed by hand. Unlike a failed job, neither reviewing it again nor adding or finishing work starts a review: the broken record stays, so the goal cannot close as `achieved`. Bring the person **Close it** (`abandoned`), or, when its work must go on, **Add work** under another goal.

Read the error the attention names, the job's files, `"$DAGQ" goal show ID --full` and `"$DAGQ" events --goal ID`, and bring the choice to the person:

- **Review it again**, once the cause is gone: `"$DAGQ" goal review ID`. The same command rearms a goal answered `keep_open`. It is the inbox's (on the person's word) or the person's, never a planner's.
- **Close it**: the person decides: `goal close ID --verdict achieved` or `abandoned` (`skills/dagq/reference/goal-close.md`).
- **Add work**: record a planning request that names the goal (`"$DAGQ" request add --text '<their words>' --ref goal:N`, `skills/dagq-inbox/reference/requests.md`); the review starts again once its tasks change.

The `approve_goal` ask is shown as an ask (`skills/dagq-inbox/reference/asks.md`). Flags: `"$DAGQ" goal review --help`, `"$DAGQ" goal close --help`.
