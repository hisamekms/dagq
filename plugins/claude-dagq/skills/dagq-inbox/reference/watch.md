# The watch

Read this when starting or restarting the inbox's background watch (step 2).

## One command in the background

Pass the cursor from `status --role inbox` (or from the last `watch` that returned) and run this one command with the Bash tool's `run_in_background`:

```sh
"$DAGQ" watch --role inbox --until-attention --after <cursor>
```

- `--until-attention` gives `watch` no timeout: it reads the queue every `--interval` (2 seconds) for as long as it takes and returns only when an attention event for the inbox arrives or the supervisors' health changes. Its completion notice reaches you only when there is something to handle.
- Do not wrap it in a shell loop, and do not add `--timeout` (the two are refused together). The command does the waiting itself, so there is no empty return to skip and no cursor to carry.
- It prints one JSON object (`events`, `supervisors_changed`, `supervisors`, `cursor`) and exits 0. Handle it (steps 3 and 4), then start the command again from its `cursor`.
- A non-zero exit is a failure, never "nothing new": the queue could not be opened or read (its error JSON goes to stderr), or the arguments were refused (a `dagq` too old for `--until-attention` refuses it). `watch` does not retry; report the error to the person.

## One watch, never a status poll

Keep exactly one watch running: start a new one only after the previous one has ended, and never beside it. Never poll `status` in a loop instead; `watch` is the only wait. After compaction or `/clear`, check the session's background tasks first: if a watch is still running, let its notice arrive instead of starting a second one.

## The throughput review's notice

`report the review` (`throughput_review_reported`, a queue event with no task) is the supervisor's throughput review of the last whole hour, of yesterday or of the ISO week before (ADR-t996-1). It asks nothing: there is no ask to answer or close, and it does not stay in `status`.

- Show the person its `mode`, `period` and `conclusion` lines as they are, with `path` (the whole review, `review.md` under `<queue dir>/reports/reviews/`). An hourly one also has `reasons`: `deviation` (the hour's landings far off the 6 hours before), `sustained_drop` (the 3-hour average well below the 24-hour one for 3 hours) or `no_landing`.
- A weekly one with `finding_id` proposed one change: it is a finding marked for a proposal, which a runtime planner takes up. Tell the person its ID; nothing is yours to do.
- Only an hour the runtime's rules flag reaches you. Say nothing about the quiet hours, and run no hourly, daily or weekly review of your own (a cron of the inbox's that did it before the runtime did is to be stopped).
- `check the failed review` (`throughput_review_finished` with `outcome` `failed` or `error`, task 1099) is a review of any mode that reached no conclusion. Show the person its `mode`, `period`, `outcome`, `reason` (the error) or `exit_code`, and `dir` (its `output.out` and `output.err` say why; older jobs have `output.log`). It asks nothing and holds no claim nor landing; the period is not reviewed again, and you run none of your own in its place unless the person says so. A Codex review that stopped because Codex could not be used (its finish has `provider_unusable`) is no notice: the supervisor holds Codex and reviews that period again on the other provider, and only under `--no-claude` does that second try end as a `check the failed review` saying why (task 1220).
