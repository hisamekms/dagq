# The watch

Read this when starting or restarting the inbox's watch (step 2). Its flags and output: `"$DAGQ" watch --help`. On Claude Code it runs in the background; on Codex, which has no background task, in the foreground ("On Codex" below).

## One command in the background

Pass the cursor from `status --role inbox` (or from the last `watch` that returned) and run this one command with the Bash tool's `run_in_background`:

```sh
"$DAGQ" watch --role inbox --until-attention --after <cursor>
```

- It returns only when there is something to handle. Do not wrap it in a shell loop and do not add `--timeout`.
- When it returns, handle the events it returns (steps 3 and 4), then start the command again from the cursor it returns.
- `report the update` and an hourly `report the review` may arrive late, with another return; the delay is no failure. When telling `report the update`, add that sessions reopened after it load the updated plugin.
- A non-zero exit is a failure, never "nothing new": report the error to the person, but `unreachable` as below.
- A watch that cannot reach the queue's service (`unreachable`) says what to do next; it is news to handle, not a failed or empty watch. While the service is down the queue holds no attention, so this is how its absence reaches you; `queue_service.outage`, when present, is the supervisor's record of it (since when, why). Run `"$DAGQ" service status` first: when it is running now, the service was only being replaced; start the watch again from the same cursor and tell no one. Otherwise tell the person, show them that status, start the service again only on their word (`up`, `dagq-recover`'s `reference/up-down.md`), then start the watch again from the same cursor. A watch that returns no events during a recorded outage: start it again from the same cursor and tell no one. Do not restart it in a loop while the service is down. With no supervisor either (after `down`, for one) there is no record: the person runs `up`. After a recorded outage, once a supervisor sees the service answer again, `queue_service_down` arrives as an attention (step 4).
- `unauthenticated` with an error that says to open the inbox again: this inbox cannot use the service as it is. Tell the person to open the inbox again (`dagq-recover`'s `reference/up-down.md`, "Open the inbox again").

## One watch, never a status poll

Keep exactly one watch running: start a new one only after the previous one has ended. Never poll `status` in a loop instead. After compaction or `/clear`, check the session's background tasks first: if a watch is still running, let its notice arrive.

## On Codex: the watch in the foreground

Codex has no background task, so the watch is bounded and runs in the foreground of your turn:

```sh
<dagq> watch --role inbox --after <cursor>
```

- `<dagq>` is the path your instructions name. Use the cursor of the status the start hook printed, then the cursor each watch returns.
- It returns within its timeout (no `--until-attention`), with events or with none. Handle what it returns (steps 3 and 4), then run it again from its cursor. An empty return is no failure and is told to no one.
- Keep doing so whenever the person is not talking to you. When they are, answer them, then run the watch again; what came meanwhile waits for it. A watch is never left running past your turn.
- The Stop hook keeps your turn from ending while no watch runs, and names the command to run. If a watch just returned output you have not handled, handle it first.
- A non-zero exit, `unreachable` and `unauthenticated`: as above, with the watch run again in the foreground.

## The throughput review's notice

`report the review` is the supervisor's review of the last hour, of yesterday or of the week before. It asks nothing and does not stay in `status`.

- Show the person its conclusion as it is, with where the whole review is.
- A weekly one that names a finding proposed a change that a runtime planner takes up: tell the person its ID; nothing is yours to do.
- A quiet hour never reaches you. Say nothing about quiet hours, and run no review of your own.
- `check the failed review`: a review that reached no conclusion. Show the person what it says and where its output files are (they say why). Run none in its place unless the person says so.

## The observer's failure notice

`check the failed observer` says the observer failed repeatedly; it comes once per run of failures and does not stay in `status`.

- Show the person what it says and where its output files are.
- It asks nothing; the supervisor starts the next observation by itself. Run no `observe` of your own unless the person says so.
- `"$DAGQ" observe --history` shows the observations since (`reference/observer.md` of the `dagq` skill).
