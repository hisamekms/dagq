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
