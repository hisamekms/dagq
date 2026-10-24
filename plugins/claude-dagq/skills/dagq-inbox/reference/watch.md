# The watch

Read this when starting or restarting the inbox's background watch (step 2). Its flags and output: `"$DAGQ" watch --help`.

## One command in the background

Pass the cursor from `status --role inbox` (or from the last `watch` that returned) and run this one command with the Bash tool's `run_in_background`:

```sh
"$DAGQ" watch --role inbox --until-attention --after <cursor>
```

- It returns only when there is something to handle. Do not wrap it in a shell loop and do not add `--timeout`.
- When it returns, handle its `events` (steps 3 and 4), then start the command again from its `cursor`.
- `report the update` and an hourly `report the review` may arrive late, with another return; the delay is no failure. When telling `report the update`, add that sessions reopened after it load the updated plugin.
- A non-zero exit is a failure, never "nothing new": report the error to the person, but `unreachable` as below.
- A watch that cannot reach the queue's service ends at once with `queue_service.code` `unreachable`; its error says what to do next, and `queue_service.outage` is the outage the supervisor recorded, if any. Run `"$DAGQ" service status` first: an update replaces the service, so a `state` of `running` now means it was only being replaced; start the watch again from the same cursor and tell no one. Otherwise tell the person, show them that status, start the service again only on their word (`up`, `dagq-recover`'s `reference/up-down.md`), then start the watch again from the same cursor. The next watch waits a recorded outage out, and at its timeout returns no events with `outage_since`: start it again from the same cursor and tell no one. With no outage recorded, do not restart it in a loop while the service is down.
- `unauthenticated` with an error that says to open the inbox again: this inbox has no token of its own (it was opened by an earlier build) or its session ended. Tell the person to open the inbox again (`dagq-recover`'s `reference/up-down.md`, "Open the inbox again").

## One watch, never a status poll

Keep exactly one watch running: start a new one only after the previous one has ended. Never poll `status` in a loop instead. After compaction or `/clear`, check the session's background tasks first: if a watch is still running, let its notice arrive.

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
