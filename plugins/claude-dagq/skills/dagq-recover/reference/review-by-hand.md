# Review by hand, integrate, and a failed push

Read this for attention `review by hand` (`kind` `review_failed`), `review and integrate`, or `push main` (the `dagq-recover` skill, section 6). Everything here is done on the person's word. Never merge, rebase, cherry-pick or fast-forward a run's branch yourself: landing is the runtime's, one squash commit per task. Never read the full diff in the inbox or a planner session. The flags are in `"$DAGQ" review --help` and `"$DAGQ" integrate --help`.

## Which runs come here

The supervisor reviews every run it accepts and lands, sends back or asks `approve_landing` by itself; while it holds the run (`reviewing (runtime)`, `queued to land (runtime)`) do nothing, and `integrate` refuses it. Do not review those runs again. Only these come here:

- `review by hand`: the headless review failed (`run_dir` keeps its output; `error` names each failed job), including a review that `--no-claude` left without a provider (`code: provider_disabled`).
- `review and integrate`: a run accepted without a review.

## Steps

1. `"$DAGQ" review ID` writes the run's review material to a file and prints its `path`.
2. Start a subagent (the Agent tool) with `path` and this request: read the file; check the diff against the task's acceptance, the goal's constraints and the receipt's claims (tests, subagent review; its `e2e` is `not_applicable`, the runtime runs e2e), and flag changes the task did not ask for; list the titles of the receipt's `follow_ups`; answer `pass` or `concern`, and for `concern` at most a few reasons with file and line, never the diff itself. Add what the repository's instructions ask for.
3. e2e: `integrate` by hand skips the runtime's e2e step. When the run's validation required e2e, check that `events --run RUN --kind run_e2e_finished` has `outcome: passed` for the reviewed commit and read the run dir's `e2e-*.log`; never run the e2e yourself. Without one, tell the person that `integrate` lands it unchecked by e2e (a binary replaced by `install` or an automatic update still passes the whole suite first) and land it only on their word.
4. Report the verdict to the person. On `pass` and their word, `"$DAGQ" integrate ID`. On `concern` (the receipt or diff disagrees with the acceptance, the diff has changes the task did not ask for, or the subagent returned findings), do not land: register `"$DAGQ" ask --kind approve_landing --run <run_id> --question "<task, the reasons, head, size, follow_ups>" --option land --option send_back --option cancel`, which the supervisor applies once answered.

## After integrate

- `integrated`: report the landed commit, what it unblocked and the draft tasks from `follow_ups` (also any entry it skipped). A draft is never claimed; the runtime opens a planner for each.
- `needs_session`: nothing landed; the supervisor resumes the session and lands the run itself. Do not `integrate` it again.
- `failed`: the receipt reported `failed`; the recovery job takes the run.
- An error leaves the landing branch untouched and puts the run back with `last_error`. A checkout of the landing branch whose uncommitted changes overlap the landing is a common cause: fix it and run `integrate` again. So is a container verification whose container podman itself did not start (exit 125): that command is not judged and the run does not go to `needs_session`; fix podman (its machine, for one) and run `integrate` again.

The task's verification commands run in `integrate`, after its rebase (an `already_satisfied` run's run on a snapshot of main instead: the `dagq-inbox` skill's `reference/asks.md`, `approve_close`); before it there is no verification log, and the receipt's `tests` is the worker's own claim. Judge by the highest attempt's `integrate-<attempt>-verify-N.log`, not by whichever file is there.

## A failed push

When `push.outcome` is `failed`, or `status` shows `push main` (`kind: push_failed`), run `"$DAGQ" doctor` and read `branch` and `remote` from its `repository` field. Fix the cause with the person (a rejected non-fast-forward, credentials, or a repository setting error), then run `git push <remote> <branch>` using those values. If `repository` reports an `error`, resolve it and read the values again before pushing. A failed push never undoes the landing; the attention clears at the next successful push.

## approve_landing answers

While its `approve_landing` ask is open the run waits without a lease and `status` shows only the ask. The supervisor applies `land`, `send_back`, `cancel` and, on a failed review's ask, `retry_review` itself, each also as `<option>: <reason>`, and closes the ask; asking again for the same run returns the same ask. An answer given while the run is away (a landing recheck resumed it) applies once it waits again. Another answer is the inbox's to read and close; the runtime closes the ask of a run that ended. Never ask the person to pick a reason code. Never edit the run's worktree, `recover` it or open a session for it.
