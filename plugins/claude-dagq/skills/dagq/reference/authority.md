# Who may run what, and how it is recorded

Read this when the binary refuses a command with `<role> may not <capability> (<reason>)`, or before doing on a person's word something your role may not do. The model behind it is the repository's `docs/design/security.md`; the command-by-command table is `docs/design/authorization.md`.

## The check

Every command that changes the queue names its caller from the environment (`DAGQ_ROLE`, `DAGQ_ACTOR_ID`, `DAGQ_RUN_ID`, `DAGQ_TASK_ID`; no `DAGQ_ROLE` is the person, `user`) and is checked against a fixed, default-deny policy before it changes anything. An unknown `DAGQ_ROLE` stops every command. A refusal changes nothing, is recorded as an `authorization_denied` event with you as its actor, and prints `{"error": "<role> may not <capability> (<reason>)", "denied": {"role", "capability", "reason"}}` (a job: `reviewer may not change queue state`; the observer: `observer may not change queue state`). Reasons: `not granted` (the role never has it), `not on this resource` (not your run, task, planner or proposal, or a task already started), `not of this kind` (an ask kind your role does not open), `reserved`.

Take a refusal as the answer: never retry it under another `DAGQ_ROLE`, without it, through a path to the binary or a script, or in the database. Hand the step to the role that has it (below), or tell the person.

## What each role is refused

- **worker**: `integrate`, `answer`, `ask close`, `ready`, `cancel`, `add` / `edit` / `submit` and every other planning command, `goal ...`, `recover`, `review`, `supervise`, `observe`, `plan`, `up` / `down` / `install` / `auto-update`, `init` / `migrate` / `rebind`, `finding ...`, `mark`; and `ask`, `note`, `session` on any run or task but its own. It opens only `worker_question` asks.
- **planner**: `ready` (with or without `--bypass-review`), `goal ready`, `goal review`, `integrate`, `review`, `recover`, `supervise`, `observe`, `answer`, `ask close`, `finding record`, `session` / `session-event` of a run; any ask on a run (it opens only `planner_question`); changing or canceling a task once it is `in_progress`; withdrawing another planner's proposal. On a run it may only write a `note`.
- **review, recovery, plan review and goal review jobs**: every command that changes state. Their verdict is data the supervisor applies.
- **observer**: every state change but `finding record`, `finding resolve` and `ask --kind blocked --finding ID`.
- **inbox**: nothing a person may do (it acts on the person's word); landing and pushing themselves are the integrator's, reached through `integrate`.

## The inbox acts on the person's behalf, and the record says so

The inbox may answer every ask and carry out `dagq-recover` (`integrate`, `recover`, `review`, `ready --bypass-review`, `cancel`, `up` / `down` / `install`, ...) on the person's word. The record keeps it apart from the person's own action:

- each event's `actor` (`events --full`, `show --full`) has role `inbox` for what the inbox did and `user` for what the person typed; a landing it asked for has `requested_by: inbox`
- an answer's `authority` (`ask_answered`) is `delegated` from the inbox, `user` from the person, `runtime` when the runtime closed it; `approval: true` marks an answer that approves (landing, plan, goal, update, a `decide`, a finding's `propose` / `dismiss`)

A `!` command typed in the inbox's terminal inherits `DAGQ_ROLE=inbox` and is recorded as delegated. To record an action as the person's own, the person types it in a terminal without `DAGQ_ROLE`.

## Advisory, not a sandbox

Everything runs as one user on the host (`status` and `doctor` show `actors` with `backend: host`, `enforcement: advisory`, `sandboxed: false`; once a supervisor can run Codex, the worker row's `providers` shows the Codex worker `confined`: its OS sandbox stops writes and signals, but it is not isolated). The check stops mistakes and records who did what; a process that fakes the environment or opens the database gets past it. That is why working around a refusal is never allowed: it is not isolated, only trusted to hold.
