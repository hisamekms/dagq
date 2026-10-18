# Who may run what, and how it is recorded

Read this when the binary refuses a command, or before doing on a person's word something your role may not do.

## The check

Every command that changes the queue names its caller from the environment (`DAGQ_ROLE`; none is the person, `user`; an unknown one stops every command) and is checked against a default-deny policy first. A refusal (`<role> may not ...`) changes nothing and is recorded with you as its actor.

Take a refusal as the answer: never retry it under another `DAGQ_ROLE`, without it, through a path to the binary or a script, or in the database. Hand it to the role that has it (below), or tell the person.

## Client mode: a worker's and a job's dagq

A worker, a job and the observer get the queue service's address and a token, never the queue's path: their `dagq` sends each command to the service, which checks it for the token's principal whatever `DAGQ_ROLE` says. A refusal names a code:

- `no_use_case`: the service does not serve the command; `queue_named`: it names a queue (`--db`). Never add `--db` to reach the queue; hand the step on.
- `authorization_denied`: refused for your principal, as in "The check".
- `unreachable`, `unauthenticated`, `no_credential`, `client_misconfigured`: no service, token or usable address. Do not work around it (the client never opens the queue instead): try again later, or write it in the receipt.
- `bad_request`, `failed`, `api_version_mismatch`: read the message.

Reads print what the command line prints.

## What each role is refused

`judge-follow-up` (`follow_up.judge`) is granted only to the person (`user`), inbox and runtime planner, for follow_up tasks in any state. It moves membership only in `draft` / `ready`; workers, jobs and the supervisor may not judge. Judgement and movement grant no adoption and preserve source task/run/goal and depth.

`agent eval <agent>` asks the supervisor for one round of an agent's eval, which it runs itself: nobody runs the agent's LLM CLI for it. A `dev` round (`eval.request`) is the person's, the inbox's, a planner's and a worker's (of its own run only); `--split holdout|production` (`eval.request_held_out`) is the person's, the inbox's and a planner's; `--rerun` of a held-out set already run (`eval.rerun`) only the person's and the inbox's. `--set ID` runs a production set of the queue's side whole as production, never with `--cases`. `agent results` / `agent result` (`eval.read`) read the rounds: a worker only its own run's. The supervisor runs rounds and asks for none.

`agent cases <agent>` asks the supervisor to make a production set of the agent's cases from its production reviews, labeled by another provider's job, and `agent decide` records a person's label of a disputed case (`eval.cases`): the person's, the inbox's and a planner's only. `agent sets` / `agent set` (`eval.read`) read the sets. The supervisor makes the sets it is asked for and asks for none.

`replan request --run RUN` (`replan.request`) asks the runtime to replan a long-running run: only the person and the inbox. `replan propose --run RUN` (`replan.propose`) is a worker's proposal on its own run only, through the queue service in client mode; it stops and changes nothing. Parking a run and applying its replacement (`replan.park`, `replan.apply`) are the supervisor's alone, with no command. While a run's replanning holds its task, `ready`, `edit`, `cancel`, dependency changes, `integrate` and a resume of it are refused for every role, the person's included.

`hold TASK|RUN --reason '...'` and `release TASK|RUN` (`task.hold`; a run's ID names its task) hold a task so that the runtime neither claims it nor resumes or recovers its run, and let it go: only the person and the inbox. A held task keeps its status, and a turn already going is not stopped.

`setup-review` (`review.setup`) has a coding agent generate the review's settings in a temporary checkout and writes them to the working tree only at the person's approval (`reference/review-setup.md`): only the person and the inbox. Approving beforehand with `--yes` (`review.setup_approve`) is the person's alone: the inbox and every other actor are refused it. Its job (`setup-review-job`) may only read the queue: every command that changes state is refused.

`revisit` (a draft's revisit time) is a planning command (`task.write`): the person, the inbox and a planner may set, change or clear it on a `draft`; a planner may not give one to a draft whose runtime planners are used up (only the person or the inbox does).

- **worker**: `integrate`, `answer`, `ask close`, `ready`, `cancel`, `add` / `edit` / `submit` / `judge-follow-up` / `revisit` and every other planning command, `goal ...` (but `goal list` / `goal show`), `request add` / `request decline`, `replan request`, `hold` / `release`, `recover`, `review`, `supervise`, `observe`, `plan`, `up` / `down` / `inbox` / `install` / `auto-update`, `init` / `migrate` / `rebind`, `setup-review`, `mark`, `session` (its own run's is the wrapper's), `watch`, `report` / `graph --out`, `ci failures` (`ci.read`) (in client mode: `no_use_case`); `finding ...`, and `ask`, `note` and `replan propose` on anything but its own run or task (`authorization_denied`); `agent eval` with `--split holdout|production`, `--set` or `--rerun`, `agent cases` / `agent decide`, or `agent result` of another run's round (`authorization_denied`). It opens only `worker_question` asks.
A priority a person set (on a goal, or on what a task takes) only the person and the inbox change. Any other actor's `set-priority` (`--inherit` too) and `goal edit --priority` of it, and a `goal add` / `add` priority other than the request's, are refused (`authorization_denied`, `a person's priority`); plan review leaves its `lower_priority` unapplied. A move by another actor (`set-goal`, a follow_up's `out_of_scope`) is not refused. With `set-goal` the task keeps the value as its own, a person's, unless the new goal has the same value as a person's. A follow_up moved by its `out_of_scope` judgement takes the new goal's instead of a value it only took from its old goal; a person's priority of the task's own stays on any move.

- **planner**: `ready` (with or without `--bypass-review` or `--inherit`), `goal ready`, `goal review`, `integrate`, `review`, `recover`, `supervise`, `observe`, `answer`, `ask close`, `finding record`, `request add` (the inbox's and the person's), `setup-review`, `replan request` / `replan propose`, `hold` / `release`, `request decline` and `ask --kind planner_question --request N` of a request it was not opened for (only the request's own planner declines it or asks on it; another planner, of another request, a draft or a finding, is refused with `authorization_denied` and no ask opens), a session's screen and log (`run screen` / `run log` / `run send`, `planner screen` / `planner log` / `planner send` / `planner request`), `session` / `session-event` of a run, `run close-workspaces`; any ask on a run (it opens only `planner_question`, on a task, a finding or its own request); changing or canceling a task once it is `in_progress` (except recording a follow_up membership judgement with `judge-follow-up`); `revisit` of a task that is not a `draft`, or of a draft whose runtime planners are used up; withdrawing another planner's proposal; `agent eval --rerun`; changing a person's priority (above). On a run it may only write a `note`.
- **review, recovery, plan review, goal review, throughput review and setup-review jobs**: every command that changes state (`setup-review` too), and `ci failures` (`ci.read`) (in client mode: `no_use_case`, and `authorization_denied` for `note`, `ask`, finding writes and `agent eval`, `results`, `result`, `cases` and `decide`). Their verdict is data the supervisor applies; the setup-review job's files are data `setup-review` checks before showing them.
- **observer**: every state change but `finding record`, `finding resolve` and `ask --kind blocked --finding ID` (in client mode: `finding dismiss`, `note`, other asks, `agent eval`, `agent cases` and `agent decide` are `authorization_denied`, the rest `no_use_case`). It holds `ci failures` (`ci.read`), which is not a queue service use case, so in client mode it is `no_use_case` too.
- **inbox**: nothing a person may do (it acts on the person's word) but `setup-review --yes`; landing and pushing themselves are the integrator's, reached through `integrate`. It registers no work itself: a plan the person asks for goes to a runtime planner as a request (`request add`, `skills/dagq-inbox/reference/requests.md`).

`plan` opens no planner for anyone: people no longer open planners, and it fails with a pointer to the inbox's request. A person at a terminal without `DAGQ_ROLE` may still `request add` and run the planning commands directly.

## The inbox acts on the person's behalf, and the record says so

The inbox acts on the person's word (answers, requests, `dagq-recover`), and the record keeps what it did apart from what the person did themselves. A `!` command in the inbox's terminal is the inbox's; to record an action as the person's own, the person types it in a terminal without `DAGQ_ROLE`.

## Advisory, not a sandbox

Everything runs as one user on the host (a worker put in a container never falls back to the host). The check stops mistakes and records who did what, but a process faking the environment or opening the database gets past it: so never work around a refusal.
