# A worker's provider and route, and falling back

Read this to choose which agent runs a task's worker and how, to read which one actually ran, or to tell a person what to do when a provider cannot be used.

## Provider and route

A task's worker has a **provider** (`claude` or `codex`) and a **route** (`worker_mode`).
The only route is `headless`: one non-interactive call per turn, whose exit and output are the turn's result; an answer, a send-back or a resume is the next call on the same session id.
`add` and `edit` refuse `--interactive`, and a task or run shown as `interactive` is still claimed and resumed headless.
The task's provider changes only its worker.
A headless job runs on Codex only when its role's table in `dagq.toml` says `provider = "codex"` (a run review agent's own `[review.subagents.<agent>]` table can say so for that agent's job alone), and runs in Codex's read-only sandbox.
Such a job moves to Claude while Codex cannot be used (missing, does not start, or held after a job stopped at its login or usage limit), and Codex's hold opens no ask.
A job that stopped because Codex could not be used starts again on Claude instead of reaching a person; its other failures go to a person as Claude's do.
A Codex review or plan review does not wait for Claude's `queue_hold` ask.
Under `--no-claude` no job falls back to Claude: a run review with no provider left goes to a person in an `approve_landing` ask, a plan review to `plan review by hand`, and a recovery job to a person (`triage_failed`, or the alert's `recovery_failed` ask), while a throughput review or an observation records why it did not run.
Planners and the inbox stay on Claude.
`dagq doctor`'s `roles` shows each role's provider and where it comes from; a role that cannot run on Codex makes `provider = "codex"` an error in `dagq.toml`.

`add` takes the worker's provider and route, and `edit ID` changes them on a draft or submitted task (a ready one goes back with `draft ID` first); what each flag does is in `--help`.
A task given no route follows the default, also when the default changes; a named route stays.
`show` prints the resolved route either way.

### Which to choose (planner)

- No reason given: give neither flag. The worker runs Claude headless, the default.
- The person wants the task done on Codex: `--provider codex`. Codex does no subagent review: its run needs no `subagent_review` evidence (the supervisor's review job still reviews it before landing). A Codex worker asks as any worker does: its `dagq ask` inside the sandbox goes to the queue service, which opens the `worker_question` at once for the run's worker.
- The person wants to watch the worker or step in: there is no route for it. Its turns are read with `"$DAGQ" run log RUN --follow`, and it reaches a person only through its asks, whose answers are its next turn.
- Never pick a provider to route around a login or a usage limit: the runtime falls back by itself (below).

## Reading which ran

- `show ID`: the task's provider and route, and each run's requested provider (the task's) and actual provider (the one running now, after any switch).
- `timeline RUN`: the run's providers and its switches.
- `events --run RUN --full`: the claim, each turn's request, start and end, and each switch (`provider_switched`) or wait for a provider (`provider_waiting`); what their fields mean is in `--help` and the event's doc comment. The turns' own output is under the run dir's `turns/`.
- `stats` and `kpi --by provider` (`reference/kpi.md`): runs and switches per provider.
- `status` / `doctor`: what each supervisor resolved for each provider, a provider held now (`provider_hold`, until its `retry_at`), and claims deferred because a task's provider cannot be used (`claim_deferrals`).

## When a provider cannot be used

Only these switch: the executable is missing (the supervisor found no `codex`; without Claude it does not start), the agent cannot start, a login is needed, or a usage limit or rate limit.
A model error, a sandbox refusal, a stopped turn, a failed test or a `failed` receipt never switch: they go to the recovery job.

- **At the claim**: a task whose provider is missing or held is claimed on the other provider's headless route (`provider_switched`).
- **Mid-run**: the failed turn's request goes to a new session on the other provider, which reads the commits and changes made so far; the conversation is not carried over. A run switches only a few times; one that cannot switch waits (`provider_waiting`) and is not failed.
- **Held**: Claude held for login or cost is the queue's `queue_hold` ask (below). Codex held, and a Claude that cannot start, open no ask: the hold is shown in `status`'s `provider_hold` and released at its `retry_at` (the reset time a usage-limit message names, else after a while), when the next call checks again.

What reaches the person (the inbox shows it):

- **The `queue_hold` ask** (`authentication`, or `cost` with `subject: usage_limit`): Claude's login ran out or it hit its limit, or both providers are unusable. Claude-only jobs and runs that could not move wait in its `affected`. The person logs Claude (or Codex) in again or waits for the limit to reset, then answers `done` (the supervisor releases every hold, sends `continue` to the waiting runs and restarts the failed jobs) or `cancel_affected`. A Codex task already moved to Claude, or the other way, needs nothing.
- **Nothing opens** when both providers merely fail to start, or when a run used up its switches and waits for Codex's hold to end while Claude works: the runtime tries again at each hold's `retry_at`. If `status` keeps showing `provider_hold` or `provider_unavailable`, report it: the person checks the executable (`doctor`'s `providers`) and, for a missing or moved `codex`, restarts the supervisor with `--codex` (`skills/dagq-recover/reference/up-down.md`).

### Turning the fallback off

A person can stop the workers' move to the other provider in the repository's `dagq.toml` `[provider_fallback]` table, and separately that of the headless jobs whose role, or whose run review agent's table, names a provider; with no table both move.
The supervisor reads it again every pass, so it takes effect without a restart.
Only a provider that cannot be used stops moving (missing, does not start, a login, a usage limit); `--no-claude` still sends a Claude task or job to Codex, and holds and asks are the same:

- **At the claim**: a task whose provider cannot be used is not claimed on the other one; it is deferred (`status`'s `claim_deferrals`) until its provider's hold ends.
- **Mid-run**: the run does not switch. It waits (`provider_waiting`, saying the fallback is off), and once its own provider's hold ends (`retry_at`, or `done` on Claude's `queue_hold` ask, which still opens for Claude's login or limit) the call goes again to the same session. Codex's wall opens no ask even with Claude usable.
- **Jobs**: such a job does not start on the other provider while its own cannot be used; it waits and starts on the same provider once the hold ends. A job that failed that way holds the provider that failed, as with the fallback on (Claude's login or limit opens the `queue_hold` ask, the job listed in `affected`). A run's review chooses per job: its job that stopped waits for its provider, the review's session open, while its other jobs go on and their answers are kept. A throughput review's period or an observation is not counted done and runs again on the same provider. A review agent whose own table names a provider runs its job on that provider instead of the run review's, and moves or waits by these same rules, alone. An agent that names none, and the whole review's job, follow the run review's role. Roles that name no provider are unchanged.

Whether to turn it off is the person's call; do not edit `dagq.toml` to route around a wall.

## A container worker's Claude credential

A worker whose `[roles.worker]` runs in the container holds no Claude credential: its Claude Code sends its run token to the queue broker, which puts in its place the credential a person chose for the host in `host.toml`'s `[claude_credential]` (the queue's `host.toml`, else the host-wide one; never `dagq.toml`): `kind = "setup_token"` (the long-lived token of `claude setup-token`, sent as the bearer) or `kind = "api_key"` (an API key issued in the Anthropic Console, paid by use), and `file` = the absolute path of a mode-0600 file holding it. There is no default: until a kind is chosen and its file is there and private, the container worker is not claimed (`claim_deferred` with `container_unavailable`, and the container's attention says why). `doctor`'s `claude_credential` shows the choice and its problem, never the value.

Which to choose is the person's call, and the person issues the credential and puts it in the file; an agent never issues or reads it. Using one's own subscription alone, the long-lived token is an option (the client is the official Claude Code, but Anthropic's terms do not expressly cover a proxy sending it). For an organization, a team, a shared host or work, recommend an API key. The broker reads only the chosen kind and never switches to the other: a login refused, a usage limit or an API key's balance that ran out reaches the worker untouched and is the usual Claude login or usage-limit hold (above).

## A headless run by hand

A headless run is a background process that runs only the session wrapper: there is no workspace, no agent screen, no dialog and no input box, so nothing is typed into it and no `answer_prompt` or `stuck_exit` ask opens for it.
A `worker_question` is asked and answered as usual; the supervisor sends `answer to ask N: ...` as the next turn.
A `stalled` ask offers no `intervene`: read the turns before answering it (`skills/dagq-recover/reference/stalled.md`).

A worker's and a runtime planner's session always runs as a background process with no workspace, whatever the repository's `dagq.toml` says.
`show` and `status` give the run's `background`, and `planners` a planner's.
Its `[dagq]` line of each turn (the start, the agent's text, its tools, the outcome) goes to that log instead of a terminal: read it with `"$DAGQ" run log RUN` (the run id, or the task id for its latest run) or `"$DAGQ" planner log ID`, and follow a live session with `--follow` until its wrapper ends.
The CLI finds the log from the queue, an ended run's and a closed planner's included.
Each turn's raw output stays under `turns/`, and `"$DAGQ" timeline RUN` gives the times.
