# Triage by hand, and recover by hand

Read this when the attention `triage by hand` or `recover by hand` reaches the person (the `dagq-recover` skill, section 4), or when a `decide` ask's run failed on a broken verify with its work committed. Act only on the person's word. The flags are in `"$DAGQ" ready --help`, `"$DAGQ" edit --help` and `"$DAGQ" cancel --help`.

## `triage by hand`

Attention `triage by hand` (`triage_failed`): an ended run's recovery job failed; the run stays as it is and is not tried again. One held only because its provider could not be used is not this attention: it stays `triaging (runtime)`. Read `last_error`, the job's files in the run directory and the run's turns (`"$DAGQ" run log RUN`), and bring the choice to the person (a workspace an older run opened is closed by the person in their own terminal, never `cmux` from the inbox):

- `"$DAGQ" ready ID`: run the task again from the current main (no plan review).
- `ready ID --inherit`: the same, carrying the run's committed work over (below).
- `cancel ID --reason CODE`: drop it, with the code that fits the person's reason (`skills/dagq/reference/register.md`, "Cancel reasons"). To change the work, cancel it and record a planning request that names it (`"$DAGQ" request add --text '<their words>' --ref task:N`, `skills/dagq-inbox/reference/requests.md`).

## Carry a run's work over by hand

When the person wants an ended run's committed work carried to a new run (a `retry_inherit` answer that matched no option, or one the job may no longer use): `"$DAGQ" ready ID --inherit --reason '<why, in the person's words>'`, only by user or inbox.

- It takes an `in_progress` task whose latest `failed` or `interrupted` run has no live process or lease and has commits of its own. Otherwise it is refused and changes nothing: with no commits use plain `ready ID`; while `triaging (runtime)`, wait for the round to end.
- It closes the run's open `decide` ask and does not use the task's one automatic `retry_inherit`.
- With a broken verify, fix it first (below).

## `recover by hand`

Attention `recover by hand` (`recovery_failed`): an older runtime's record of a live run's failed recovery job. Read the run's turns with the person and carry out what they decide (`reference/session.md`). It clears once the session exits.

## Broken verification command with committed work

Read `"$DAGQ" show ID --full` and the run's `integrate-<attempt>-verify-N.log`. If the command itself is wrong (it needs a tool the host lacks), ask the person for the corrected one. Only user or inbox, once the latest run has ended and no live run remains: `"$DAGQ" edit ID --verify 'correct command'` (repeat `--verify` for every command to keep; `--no-verify` clears all), check the old and new values with `show --full`, then answer the open `decide` ask with the exact option `edit the task's --verify, then retry_inherit`. The recovery job then keeps the branch's commits for the next integration, even past its tries. Answering without editing first counts like any other option: edit, then answer the ask that opens again. Never use `retry` to discard committed work for this correction. The recovery job itself does not edit tasks.
