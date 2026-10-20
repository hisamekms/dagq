# A worker's provider and route, and falling back

Read this to choose a task's worker's agent, to read which one ran, or to tell a person what to do when a provider cannot be used (flags: `"$DAGQ" add --help`).

## Provider and route

A task's worker has a **provider** (`claude` or `codex`) and a **route**; the only route is headless. `add` and `edit` refuse `--interactive`. `add` takes the provider, and `edit ID` changes it on a draft or submitted task (a ready one goes back with `draft ID` first); `show` prints the resolved provider and route.

An `in_progress` task's provider and model change only through a release of its hold: `release TASK --provider P --model M` (on the person's word, the person's or the inbox's). A run that waits (not `failed` or `interrupted`) and has a session goes on in it on the same provider, `--model` from its next turn; another provider ends the run, and the next run carries its work over. A stopped run (`failed`, `interrupted`) released with `--provider` (its own included) or `--model` always ends for a new run that carries its work over. The branches: the `dagq-recover` skill's `reference/hold.md`.

The task's provider changes only its worker. A headless job runs on Codex only when its role's table in `dagq.toml` says `provider = "codex"` (a run review agent's own `[review.subagents.<agent>]` table can say so for its job alone); planners and the inbox stay on Claude. `"$DAGQ" doctor`'s `roles` shows each role's provider and where it comes from.

### Which to choose (planner)

- No reason given: give no flag (`"$DAGQ" add --help`).
- The person wants the task done on Codex: `--provider codex`. Codex does no subagent review, so its run needs no `subagent_review` evidence (the supervisor's review still reviews it before landing). A Codex worker asks as any worker does.
- The person wants to watch the worker or step in: there is no route for it. Its turns are read with `"$DAGQ" run log RUN --follow`, and it reaches a person only through its asks.
- Never pick a provider to route around a login or a usage limit: the runtime falls back by itself (below).

## Reading which ran

- `show ID`, `timeline RUN` and `events --run RUN --full`: each run's requested and actual provider, its switches and waits.
- `stats` and `kpi --by provider` (`reference/kpi.md`): runs and switches per provider.
- `status` / `doctor`: a provider held now (`provider_hold`, until its `retry_at`) and claims deferred because a task's provider cannot be used.

## When a provider cannot be used

Only a provider that cannot be used makes a run or job move to the other one: the executable is missing, the agent cannot start, a login is needed, or a usage or rate limit. A model error, a sandbox refusal, a stopped turn, a failed test or a `failed` receipt never switch: they go to the recovery job.

What reaches the person (the inbox shows it):

- **The `queue_hold` ask**: Claude's login ran out or it hit its limit, or no provider can be used. The person logs in again (or waits for the limit to reset), then answers `done`, or `cancel_affected`. A task already moved to the other provider needs nothing.
- **Nothing opens** for Codex held or for a provider that merely fails to start: the runtime tries again at the hold's `retry_at`. If `status` keeps showing `provider_hold` or `provider_unavailable`, report it: the person checks the executable (`doctor`'s `providers`) and, for a missing or moved `codex`, restarts the supervisor with `--codex` (`skills/dagq-recover/reference/up-down.md`).
- Under `--no-claude` nothing falls back to Claude: what had no provider left goes to a person (an `approve_landing` ask, `plan review by hand`, a recovery ask) or records why it did not run.

### Turning the fallback off

A person can stop the move to the other provider, for workers and for headless jobs separately, in `dagq.toml`'s `[provider_fallback]` table; it takes effect without a restart. Then a task or job whose provider cannot be used waits for that provider's hold to end instead, and holds and asks are the same; `--no-claude` still sends Claude work to Codex. Whether to turn it off is the person's call; never edit `dagq.toml` to route around a wall.

## A container worker's Claude credential

A container worker holds no Claude credential: the queue's broker supplies the one a person chose in `host.toml`'s `[claude_credential]` (never `dagq.toml`), `kind = "setup_token"` (`claude setup-token`) or `"api_key"` (Anthropic Console), its `file` mode 0600; it never switches kinds, and claims no container worker until then (`doctor`'s `claude_credential`). Which to choose is the person's call, and the person issues it and writes its file; an agent never issues or reads it. Recommend an API key for an organization, a team, a shared host or work (Anthropic's terms do not expressly cover a proxy sending the subscription's token).

## A headless run by hand

A worker's and a runtime planner's session is a background process with no workspace: nothing is typed into it, and it opens no `answer_prompt` or `stuck_exit` ask. A `worker_question` is answered as usual and reaches the session as its next turn. A `stalled` ask offers no `intervene`: read the turns before answering it (`skills/dagq-recover/reference/stalled.md`).

Read a session's turns with `"$DAGQ" run log RUN` (the run id, or the task id for its latest run) or `"$DAGQ" planner log ID`, `--follow` for a live one; `"$DAGQ" timeline RUN` gives the times.
