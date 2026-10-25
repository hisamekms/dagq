# A worker's provider and route, and falling back

Read this to choose a task's worker's agent, to read which one ran, or to tell a person what to do when a provider cannot be used (flags: `"$DAGQ" add --help`).

## Provider and route

A task's worker has a **provider** (`claude` or `codex`); its session is always headless. `add` takes the provider, and `edit ID` changes it on a draft or submitted task (a ready one goes back with `draft ID` first).

An `in_progress` task's provider and model change only through a release of its hold, on the person's word (the `dagq-recover` skill's `reference/hold.md`).

The task's provider changes only its worker. A headless job's provider is the repository's setting for its role; planners stay on Claude. The inbox's provider (`claude` or `codex`) and model come from its role's setting, and `dagq inbox`'s arguments override them for one launch (`dagq inbox --help`); with neither, it opens on Claude (unless Claude may not start, below). How the inbox is opened, watches and comes back: the `dagq-inbox` skill and `skills/dagq-recover/reference/up-down.md` (opening the inbox again). `"$DAGQ" doctor` shows each role's provider and where it comes from.

### Which to choose (planner)

- No reason given: give no flag (`"$DAGQ" add --help`).
- The person wants the task done on Codex: `--provider codex`. Codex does no subagent review, so do not ask its run for subagent review evidence (`--evidence`); the supervisor's review still reviews it before landing. A Codex worker asks as any worker does.
- The person wants to watch the worker or step in: there is no route for it. Its turns are read with `"$DAGQ" run log RUN`, and it reaches a person only through its asks.
- Never pick a provider to route around a login or a usage limit: the runtime falls back by itself (below).

## Reading which ran

- `show ID`, `timeline RUN` and `events --run RUN`: which provider each run asked for and ran on.
- `stats` and `kpi --by provider` (`reference/kpi.md`): runs and switches per provider.
- `status` / `doctor`: a provider held now, and claims deferred because a task's provider cannot be used.

## When a provider cannot be used

Only a provider that cannot be used makes a run or job move to the other one: the executable is missing, the agent cannot start, a login is needed, or a usage or rate limit. A model error, a sandbox refusal, a stopped turn, a failed test or a `failed` receipt never switch: they go to the recovery job.

What reaches the person (the inbox shows it):

- **The `queue_hold` ask**: Claude's login ran out or it hit its limit, or no provider can be used. The person logs in again (or waits for the limit to reset), then answers `done`, or `cancel_affected`. A task already moved to the other provider needs nothing.
- **Nothing opens** for Codex held or for a provider that merely fails to start: the runtime tries again. If `status` keeps showing it held or unavailable, report it: the person checks the executable (`doctor`) and, for a missing or moved `codex`, restarts the supervisor with `--codex` (`skills/dagq-recover/reference/up-down.md`).
- Under `--no-claude` nothing falls back to Claude: what had no provider left goes to a person (an `approve_landing` ask, `plan review by hand`, a recovery ask) or records why it did not run.
- **The inbox** is outside the fallback: the runtime does not start it, so a hold never stops an open inbox, and `dagq inbox` never quietly opens it on Claude instead. When Claude may start and Codex is chosen but cannot be used, it does not open: it says why and how to open it on Claude. While Claude may not start (the supervisor runs without Claude, above), it opens no Claude inbox: with Claude from the role's setting or the default, it opens on Codex if Codex can be chosen and used, else it does not open and says why; Claude asked for in the launch's arguments is refused with the reason. Any other check that stops a Codex inbox (such as the person's Codex settings) also says why. None of these opens an ask: the person follows the reason and runs `dagq inbox` again.

### Turning the fallback off

A person can stop the move to the other provider in the repository's `dagq.toml`; then a task or job whose provider cannot be used waits for it instead. Whether to turn it off is the person's call; never edit `dagq.toml` to route around a wall.

## A container worker's Claude credential

A container worker holds no Claude credential: the queue's broker supplies the one a person chose in the host's `host.toml` (never `dagq.toml`), a setup token (`claude setup-token`) or an API key (Anthropic Console), in a file only its owner reads; no container worker is claimed until then (`doctor`). Which to choose is the person's call, and the person issues it and writes its file; an agent never issues or reads it. Recommend an API key for an organization, a team, a shared host or work (Anthropic's terms do not expressly cover a proxy sending the subscription's token).

## A headless run by hand

A worker's and a runtime planner's session is a background process with no workspace: nothing is typed into it. A `worker_question` is answered as usual. Read the turns before answering a `stalled` ask (`skills/dagq-recover/reference/stalled.md`).

Read a session's turns with `"$DAGQ" run log RUN` or `"$DAGQ" planner log ID`; `"$DAGQ" timeline RUN` gives the times.
