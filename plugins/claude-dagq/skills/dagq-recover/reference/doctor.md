# doctor and recover: what to read and when to act

Read this when diagnosing a run by hand (the `dagq-recover` skill, sections 1–3). The flags are in `"$DAGQ" doctor --help` and `"$DAGQ" recover --help`.

## doctor --full

Plain `doctor` is the compact form; diagnosing needs `--full`.

- A stale supervisor registration is left by a killed or hung supervisor. Do not remove a registration yourself: report one that stays to the person; `recover` works regardless of it. With no supervisor listed, nobody owns any run.
- A run with nothing listed as blocking its recovery is recoverable.
- The inbox's guardrail off: `reference/up-down.md`, "Open the inbox again".
- An agent's errors: a review needing such an agent does not pass; the configuration is fixed on the landing branch.
- The CI watch: what `gh` resolved to on your PATH, which may differ from the supervisor's.

## sccache server

On the attention `stop sccache on the host; the supervisor starts it`, show the person the server's identity, health and the error from `doctor`. On their word they stop that server on the host after checking its identity and port; the supervisor starts the replacement itself. Do not start one by hand.

## When to recover

Leave to the supervisor:

- A `running` or `validating` run whose wrapper is alive under a stale lease: the next supervisor adopts it. Start or wait for one (`up`) and watch `show ID`.
- A run waiting for its session to exit under a live lease, or one the supervisor gave up while its session runs: the supervisor stops what is left itself.
- An `awaiting_integration` run under a stale lease, and a run whose supervisor and wrapper both died: the next supervisor takes it.
- A leaseless `validating` run whose `already_satisfied` claim rests: answer its `approve_close` ask instead.

`recover` is only for a run whose wrapper is dead or silent, a `claimed` / `starting` run, an `integrating` run (an `integrate` that died), or a run without a lease, and only while no supervisor runs. A supervisor alive without a heartbeat is stopped by the person with `down --force` only when no other live supervisor runs the queue; beside a live one, they check its pid and stop only that process, never `down`; a row under the live supervisor's own pid needs nothing (`reference/up-down.md`, "Two supervisors of one queue").

`recover` goes through the queue's service, and `down`, `--force` included, stops the service once no supervisor is left, so a `recover` right after it fails as `unreachable`. Before `recover`, start the service alone with `"$DAGQ" service start` and check it with `"$DAGQ" service status`; do not start it with `up`, which starts a supervisor too. Leave the service running afterwards: the next `up` uses it.

## recover

`"$DAGQ" recover RUN_ID` releases the run for the next supervisor's recovery, keeping its worktree, branch and run directory; a run left landable is landed as in `reference/review-by-hand.md`. It is refused while a process of the run is alive or its lease is fresh, and fails as `unreachable` while the service does not answer: start it alone first, as in "When to recover". With a supervisor running, leave the run to it; nothing types into a session. The person ends a hung supervisor or a session no supervisor serves in their own terminal; never kill processes unless the person asks.
