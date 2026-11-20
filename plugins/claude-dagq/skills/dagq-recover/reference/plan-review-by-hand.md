# Plan review by hand and planner attention

Read this when the attention `plan review by hand` or `check the planner` reaches the person (the `dagq-recover` skill, section 8). Act only on the person's word; `ready --bypass-review` follows the rules of section 8.

`plan review by hand`: the proposal's headless plan review failed and holds the proposal. Read the error the attention names, the job's files, `"$DAGQ" proposal show ID` and the tasks, and bring the choice to the person: have the person type `dagq ready ID --bypass-review` for each task in their own terminal without `DAGQ_ROLE` and confirm the target on the TTY (the inbox cannot relay this), send it through plan review again (`"$DAGQ" submit --proposal ID`, typed by the person in their own terminal without `DAGQ_ROLE`), or `cancel` the tasks (each with its `--reason`, `skills/dagq/reference/register.md`, "Cancel reasons"). A change to the plan goes as a planning request naming its tasks (`request add --ref task:N`, `skills/dagq-inbox/reference/requests.md`).

For `check the planner` with `planner_unresponsive`, the person checks the planner (`dagq planners`, `"$DAGQ" planner log ID`). For a runtime planner whose turn hit its limit, they can send a follow-up with `"$DAGQ" planner request ID` if the planner is open, or leave it.

For `planner_switch_stalled`, the original planner hit a provider barrier on its first turn before it could start the work. Repeated attempts to start a planner on the other provider ran out. The work returned to its usual planning route, and any answers carried in the handoff did not reach a planner.

Check the original planner (`source`), the work named in the attention, the ask IDs of the carried answers (`asks`), and the reason the last recipient (`planner_id`) ended. Read the original and recipient logs with `"$DAGQ" planner log ID` if available. The person can let normal planning proceed or give the undelivered answer as a follow-up to an open planner handling the work. If no planner can take that follow-up, on the person's word record a planning request with their answer and references to the work. See [planning requests](../../dagq-inbox/reference/requests.md).

For either notice, leave planner closure and task readiness to the runtime.
