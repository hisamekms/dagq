# The watch loop

Read this when starting or restarting the inbox's background watch (step 2).

## Stay silent on an empty timeout

`watch` returns after `--timeout` (default 600 seconds) even when nothing happened: `events` is empty, `supervisors_changed` is `false`, and `cursor` is the one it was given. Such a return is not news. Do not report it to the person and do not say "nothing new": a "nothing happened" every ten minutes is noise that buries the reports that matter. Watch again from the same cursor instead, without a turn of your own.

To get there, wrap `watch` in a shell loop that calls it again on every empty timeout and exits only when `events` is not empty or `supervisors_changed` is `true` (or `watch` itself fails). Run that loop with the Bash tool's `run_in_background`, so its completion notice reaches you only when there is something to handle.

## The loop

Pass `$DAGQ` and the cursor from `status --role inbox` (or from the last `watch` that returned something):

```sh
sh -c '
dagq=$1 cursor=$2
while :; do
  out=$("$dagq" watch --role inbox --after "$cursor") || { printf "%s\n" "$out"; exit 1; }
  news=$(printf "%s" "$out" | jq "(.events | length) > 0 or .supervisors_changed") || { printf "%s\n" "$out"; exit 1; }
  if [ "$news" = true ]; then printf "%s\n" "$out"; exit 0; fi
  cursor=$(printf "%s" "$out" | jq -r .cursor)
done
' _ "$DAGQ" <cursor>
```

- `jq` judges the output: the number of `events` and `supervisors_changed`. An empty timeout carries the cursor on (unchanged, as `watch` returns it) and loops without printing anything.
- The loop prints the one `watch` output that has something (`events`, `supervisors_changed`, `supervisors`, `cursor`) and ends. Handle it (steps 3 and 4), then start the loop again from its `cursor`.
- A failing `watch` (the queue cannot be opened; its error JSON goes to stderr) or output `jq` cannot read ends the loop with exit 1 and whatever it printed: report that to the person; it is not an empty timeout.

## One loop, never a status poll

Keep exactly one loop running: start a new one only after the previous one has ended, and never beside it. Never poll `status` in a loop instead; `watch` is the only wait. After compaction or `/clear`, check the session's background tasks first: if a loop is still running, let its notice arrive instead of starting a second one.
