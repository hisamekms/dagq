# Review by hand, integrate, and a failed push

Read this for attention `review by hand`, `review and integrate`, or `push main` (the `dagq-recover` skill, section 6). Everything here is done on the person's word. Never merge, rebase, cherry-pick or fast-forward a run's branch yourself: landing is the runtime's, one squash commit per task. Never read the full diff in the inbox or a planner session. The flags are in `"$DAGQ" review --help` and `"$DAGQ" integrate --help`.

## Which runs come here

The supervisor reviews every run it accepts and lands, sends back or asks `approve_landing` by itself; while it holds the run (`reviewing (runtime)`, `queued to land (runtime)`) do nothing. Do not review those runs again. Only these come here:

- `review by hand`: the headless review failed (its output stays in the run directory), including a review that `--no-claude` left without a provider.
- `review and integrate`: a run accepted without a review.

## Steps

1. `"$DAGQ" review ID` writes the run's review material to a file and prints the file's path.
2. Start a subagent (the Agent tool) with that path and this request: read the file; check the diff against the task's acceptance, the goal's constraints and the receipt's claims (tests, subagent review; it claims no e2e, which the runtime runs), and flag changes the task did not ask for; list the titles of the receipt's follow-ups; answer `pass` or `concern`, and for `concern` at most a few reasons with file and line, never the diff itself. Add what the repository's instructions ask for.
3. e2e: `integrate` by hand skips the runtime's e2e step. When the run needed e2e, check in `events --run RUN` and the run directory's e2e log that it passed for the reviewed commit; never run the e2e yourself. Without one, tell the person that `integrate` lands it unchecked by e2e and land it only on their word.
4. Report the verdict to the person. On `pass` and their word, `"$DAGQ" integrate ID`. On `concern` (the receipt or diff disagrees with the acceptance, the diff has changes the task did not ask for, or the subagent returned findings), do not land: register `"$DAGQ" ask --kind approve_landing --run <run_id> --question "<task, the reasons, head, size, follow_ups>" --option land --option send_back --option cancel`, which the supervisor applies once answered.

## After integrate

- `integrated`: report the landed commit, what it unblocked and the draft tasks from the receipt's follow-ups. The runtime opens a planner for each draft.
- `needs_session`: nothing landed; the supervisor resumes the session. Do not `integrate` it again.
- `failed`: the recovery job takes the run.
- An error leaves the landing branch untouched and names its cause (`show ID --full`). A checkout of the landing branch whose uncommitted changes overlap the landing is a common cause; so is a container runtime that did not start. Fix it with the person and run `integrate` again.

The task's verification commands run in `integrate`, after its rebase; before it the receipt's test evidence is the worker's own claim. Judge by the latest attempt's verification log in the run directory.

## A failed push

When `integrate` reports that its push failed, or `status` shows `push main`, run `"$DAGQ" doctor` and read `branch` and `remote` from its `repository` field. Fix the cause with the person (a rejected non-fast-forward, credentials, or a repository setting error), then run `git push <remote> <branch>` using those values. If `doctor` reports an error for the repository, resolve it and read the values again before pushing. A failed push never undoes the landing; the attention clears at the next successful push.

## approve_landing answers

The supervisor applies the ask's options itself and closes the ask; another answer is the inbox's to read and close. Never ask the person to pick a reason code. Never edit the run's worktree, `recover` it or open a session for it.
