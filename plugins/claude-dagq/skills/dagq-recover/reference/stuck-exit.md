# Read a past `stuck_exit` ask

No `stuck_exit` ask opens any more. A run's session is headless and runs in the background: the supervisor asks it to exit by writing the exit request into the run directory's `turns/` (the wrapper stops a running turn and exits), and stops what is left of the wrapper itself once the session ended. Nothing types `/exit`, answers a dialog or reads a screen, and no recovery job runs for a stuck exit.

A `stuck_exit` ask in the records came from the retired interactive session: its `/exit` did not end the session within the exit timeout (`exit_request_timed_out`), and its options were `exit` and `wait`. Read it as history: the ask and its answer with `"$DAGQ" asks --role inbox` (`--all` for closed ones), the run with `"$DAGQ" show <task_id> --full`, and `"$DAGQ" events --run RUN --full` for `exit_request_timed_out`, `exit_retried` and `session_exited`. No command types into a run's session now (`run send` refuses every run), so nothing of it is carried out.

## When the session already exited

The supervisor closes such an ask itself once it sees that session's exit (`the session exited; closed by the runtime`) and moves the run on by its status, as for any ended session. Nothing to do. If the ask is still listed as answered and not closed, `"$DAGQ" ask close <id>`.

## Answer `exit` (what it meant)

The answer once had the inbox end the session by hand. On the confirmation `"Background work is running"` it checked that the worktree was clean (`git -C <worktree_path> status --porcelain` printed nothing) and that `jq -r .commit <receipt_path>` equals `git -C <worktree_path> rev-parse HEAD`, and only then would select "Exit and stop tasks"; otherwise it typed `/exit`. None of that can be done now: there is no screen and no key to send. Tell the person what the answer asked and where the run stands; on their word `"$DAGQ" ask close <id>`. A session of that kind still alive is ended only by the person, in their own terminal.

## Answer `wait`, or anything else

Do only what the answer says, on the person's word; `wait` needs nothing. Then `"$DAGQ" ask close <id>` to mark the answer read.
