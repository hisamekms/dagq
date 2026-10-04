# Who may run what, and how it is recorded

Read this when the binary refuses a command with `<role> may not <capability> (<reason>)` or with a `queue_service` code (`{"error": ..., "queue_service": {"code": ...}}`, a worker's or a job's `dagq` in client mode), or before doing on a person's word something your role may not do.

## The check

Every command that changes the queue names its caller from the environment (`DAGQ_ROLE`, `DAGQ_ACTOR_ID`, `DAGQ_RUN_ID`, `DAGQ_TASK_ID`; no `DAGQ_ROLE` is the person, `user`) and is checked against a fixed, default-deny policy before it changes anything. An unknown `DAGQ_ROLE` stops every command. A refusal changes nothing, is recorded as an `authorization_denied` event with you as its actor, and prints `{"error": "<role> may not <capability> (<reason>)", "denied": {"role", "capability", "reason"}}` (a job: `reviewer may not change queue state`; the observer: `observer may not change queue state`). Reasons: `not granted` (the role never has it), `not on this resource` (not your run, task, planner or proposal, or a task already started), `not of this kind` (an ask kind your role does not open), `reserved`.

Take a refusal as the answer: never retry it under another `DAGQ_ROLE`, without it, through a path to the binary or a script, or in the database. Hand the step to the role that has it (below), or tell the person.

## Client mode: a worker's and a job's dagq

The runtime gives the worker (and its resume), the review, recovery, plan review, goal review and throughput review jobs and the observer the queue service's socket (`DAGQ_SERVICE_SOCKET`) and a token's file (`DAGQ_SERVICE_CREDENTIAL_FILE`), never the queue's path. Their `dagq` then opens no queue: it sends each command to the service, which checks it against the same policy for the token's principal (the run's worker, the job), whatever `DAGQ_ROLE` says. A refusal exits 1 and prints `{"error": "<message>", "queue_service": {"code": "<code>"}}`:

- `no_use_case`: the command is none of the service's use cases (`init`, `add`, `edit`, `submit`, `ready`, `cancel`, `goal add`, `integrate`, `answer`, `ask close`, `mark`, `recover`, `review`, `up`, `watch`, `report`, `doctor`, ...). Refused by the client; nothing is recorded.
- `queue_named`: the command names a queue (`--db`). Refused by the client; nothing is recorded. Never add `--db` to reach the queue.
- `authorization_denied`: the service refused it for your principal (a `note` or `ask` outside your run and task, a finding write that is not your role's); the message is `<role> may not <capability> (<reason>)`, and it is recorded as `authorization_denied` with you as its actor.
- `unauthenticated`: no token, one never issued or revoked, or its run has ended; recorded as `queue_service_unauthenticated`. `no_credential`: the token's file does not read; nothing is recorded.
- `unreachable`: no service answers on the socket; nothing is recorded, and the client does not open the queue instead. Do not work around it: wait and try again later, or write it in the receipt.
- `bad_request`, `failed`, `api_version_mismatch`: the params do not read, the use case failed (a missing task, ...), or the service speaks another API version.

`locate` (the launcher's `--resolve`) answers `client_mode: true` with the socket and no `db`. Reads (`show`, `list`, `events`, `stats`, `kpi`, ...) print what the command line prints.

## What each role is refused

`judge-follow-up` (`follow_up.judge`) is granted only to the person (`user`), inbox and runtime planner, for follow_up tasks in any state. It moves membership only in `draft` / `ready`; workers, jobs and the supervisor may not judge. Judgement and movement grant no adoption and preserve source task/run/goal and depth.

- **worker**: `integrate`, `answer`, `ask close`, `ready`, `cancel`, `add` / `edit` / `submit` / `judge-follow-up` and every other planning command, `goal ...` (but `goal list` / `goal show`), `request add` / `request decline`, `recover`, `review`, `supervise`, `observe`, `plan`, `up` / `down` / `install` / `auto-update`, `init` / `migrate` / `rebind`, `mark`, `session` (in client mode: `no_use_case`); `finding ...`, and `ask` and `note` on any run or task but its own (`authorization_denied`). It opens only `worker_question` asks.
- **planner**: `ready` (with or without `--bypass-review`), `goal ready`, `goal review`, `integrate`, `review`, `recover`, `supervise`, `observe`, `answer`, `ask close`, `finding record`, `request add` (the inbox's and the person's), `request decline` of a request it was not opened for, a session's screen and log (`run screen` / `run log` / `run send`, `planner screen` / `planner log` / `planner send` / `planner request`), `session` / `session-event` of a run; any ask on a run (it opens only `planner_question`, on a task, a finding or its request); changing or canceling a task once it is `in_progress` (except recording a follow_up membership judgement with `judge-follow-up`); withdrawing another planner's proposal. On a run it may only write a `note`.
- **review, recovery, plan review, goal review and throughput review jobs**: every command that changes state (in client mode: `no_use_case`, and `authorization_denied` for `note`, `ask` and finding writes). Their verdict is data the supervisor applies.
- **observer**: every state change but `finding record`, `finding resolve` and `ask --kind blocked --finding ID` (in client mode: `finding dismiss`, `note` and other asks are `authorization_denied`, the rest `no_use_case`).
- **inbox**: nothing a person may do (it acts on the person's word); landing and pushing themselves are the integrator's, reached through `integrate`. It registers no work itself: a plan the person asks for goes to a runtime planner as a request (`request add`, `skills/dagq-inbox/reference/requests.md`).

`plan` opens no planner for anyone: people no longer open planners (ADR-t1394-1), and it fails with a pointer to the inbox's request. A person at a terminal without `DAGQ_ROLE` may still `request add` and run the planning commands directly.

## The inbox acts on the person's behalf, and the record says so

The inbox may answer every ask, record a planning request (`request add`), hand an open planner a follow-up (`planner request`) and carry out `dagq-recover` (`integrate`, `recover`, `review`, `ready --bypass-review`, `cancel`, `up` / `down` / `install`, ...) on the person's word. The record keeps it apart from the person's own action:

- each event's `actor` (`events --full`, `show --full`) has role `inbox` for what the inbox did and `user` for what the person typed; a landing it asked for has `requested_by: inbox`
- an answer's `authority` (`ask_answered`) is `delegated` from the inbox, `user` from the person, `runtime` when the runtime closed it; `approval: true` marks an answer that approves (landing, plan, goal, update, a `decide`, a finding's `propose` / `dismiss`)

A `!` command typed in the inbox's terminal inherits `DAGQ_ROLE=inbox` and is recorded as delegated. To record an action as the person's own, the person types it in a terminal without `DAGQ_ROLE`.

## Advisory, not a sandbox

Everything runs as one user on the host (`status` and `doctor` show `actors` with `backend: host`, `enforcement: advisory`, `sandboxed: false`; once a supervisor can run Codex, the worker row's `providers` shows the Codex worker `confined`: its OS sandbox stops writes and signals, but it is not isolated). The check stops mistakes and records who did what; a process that fakes the environment or opens the database gets past it. That is why working around a refusal is never allowed: it is not isolated, only trusted to hold.
