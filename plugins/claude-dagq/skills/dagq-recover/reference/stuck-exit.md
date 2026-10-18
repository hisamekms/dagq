# Read a past `stuck_exit` ask

No `stuck_exit` ask opens any more: the supervisor ends a headless session through its exit request and stops what is left of the wrapper itself. One in the records came from the retired interactive session, whose `/exit` did not end it in time. Read it as history: the ask with `"$DAGQ" asks --role inbox` (`--all` for closed ones), the run with `"$DAGQ" show <task_id> --full` and its events with `"$DAGQ" events --run RUN --full`. Nothing of it is carried out.

## When the session already exited

The supervisor closes such an ask itself. If it is still listed as answered and not closed, `"$DAGQ" ask close <id>`.

## Answer `exit` (what it meant)

The answer once had the inbox end the session by hand: on the confirmation `"Background work is running"` it checked that `git -C <worktree_path> status --porcelain` printed nothing and that `jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`, and only then would select "Exit and stop tasks". None of that can be done now. Tell the person what the answer asked and where the run stands; on their word `"$DAGQ" ask close <id>`. A session of that kind still alive is ended only by the person, in their own terminal.

## Answer `wait`, or anything else

Do only what the answer says, on the person's word; `wait` needs nothing. Then `"$DAGQ" ask close <id>`.
