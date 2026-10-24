# Plan review by hand, and an unresponsive planner

Read this when the attention `plan review by hand` or `check the planner` reaches the person (the `dagq-recover` skill, section 8). Act only on the person's word; `ready --bypass-review` follows the rules of section 8.

`plan review by hand`: the proposal's headless plan review failed and holds the proposal. Read the error the attention names, the job's files, `"$DAGQ" proposal show ID` and the tasks, and bring the choice to the person: `ready --bypass-review` each task, send it through plan review again (`"$DAGQ" submit --proposal ID`, typed by the person in their own terminal without `DAGQ_ROLE`), or `cancel` the tasks (each with its `--reason`, `skills/dagq/reference/register.md`, "Cancel reasons"). A change to the plan goes as a planning request naming its tasks (`request add --ref task:N`, `skills/dagq-inbox/reference/requests.md`).

`check the planner`: the person looks at that planner (`dagq planners`, `"$DAGQ" planner log ID`); nothing is closed or readied for it. A runtime planner whose turn was stopped at its limit: the person hands it a follow-up (`"$DAGQ" planner request ID`) or leaves it. Nothing is typed into a planner.
