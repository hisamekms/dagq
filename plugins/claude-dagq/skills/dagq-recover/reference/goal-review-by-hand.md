# Goal review by hand

Read this when the attention `goal review by hand` reaches the person (the `dagq-recover` skill, section 8). Act only on the person's word.

A finished goal is judged by the supervisor's headless goal review job, not by a planner (`skills/dagq/reference/goal-close.md`). Attention `goal review by hand`: that job failed and the goal stays open.

Read the error the attention names, the job's files, `"$DAGQ" goal show ID --full` and `"$DAGQ" events --goal ID`, and bring the choice to the person:

- **Review it again**, once the cause is gone: `"$DAGQ" goal review ID`. The same command rearms a goal answered `keep_open`. It is the inbox's (on the person's word) or the person's, never a planner's.
- **Close it**: the person decides: `goal close ID --verdict achieved` or `abandoned` (`skills/dagq/reference/goal-close.md`).
- **Add work**: record a planning request that names the goal (`"$DAGQ" request add --text '<their words>' --ref goal:N`, `skills/dagq-inbox/reference/requests.md`); the review starts again once its tasks change.

The `approve_goal` ask is shown as an ask (`skills/dagq-inbox/reference/asks.md`). Flags: `"$DAGQ" goal review --help`, `"$DAGQ" goal close --help`.
