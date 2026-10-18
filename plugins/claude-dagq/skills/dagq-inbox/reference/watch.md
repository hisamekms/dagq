# The watch

Read this when starting or restarting the inbox's background watch (step 2). Its flags and output: `"$DAGQ" watch --help`.

## One command in the background

Pass the cursor from `status --role inbox` (or from the last `watch` that returned) and run this one command with the Bash tool's `run_in_background`:

```sh
"$DAGQ" watch --role inbox --until-attention --after <cursor>
```

- It has no timeout and returns only when there is something to handle. Do not wrap it in a shell loop and do not add `--timeout`.
- When it returns, handle its `events` (steps 3 and 4), then start the command again from its `cursor`.
- `report the update` and an hourly `report the review` do not wake it alone; they come with the next return. The time they waited is no failure. When telling `report the update`, add that sessions reopened after it load the updated plugin.
- A non-zero exit is a failure, never "nothing new": report the error to the person.

## One watch, never a status poll

Keep exactly one watch running: start a new one only after the previous one has ended. Never poll `status` in a loop instead. After compaction or `/clear`, check the session's background tasks first: if a watch is still running, let its notice arrive.

## The throughput review's notice

`report the review` is the supervisor's review of the last hour, of yesterday or of the week before. It asks nothing and does not stay in `status`.

- Show the person its `mode`, `period` and `conclusion` lines as they are, with `path` (the whole review), and an hourly one's `reasons`.
- A weekly one with `finding_id` proposed a change that a runtime planner takes up: tell the person its ID; nothing is yours to do.
- A quiet hour never reaches you. Say nothing about quiet hours, and run no review of your own.
- `check the failed review`: a review that reached no conclusion. Show its `mode`, `period`, `outcome`, `reason` or `exit_code`, and `dir` (its output files say why). Run none in its place unless the person says so.

## The observer's failure notice

`check the failed observer` says the observer failed twice in a row; it comes once per run of failures and does not stay in `status`.

- Show the person its `mode`, `outcome`, `consecutive_failures`, `reason` or `exit_code`, and `dir`.
- It asks nothing; the supervisor starts the next observation by itself. Run no `observe` of your own unless the person says so.
- `"$DAGQ" observe --history` shows the observations since (`reference/observer.md` of the `dagq` skill).
