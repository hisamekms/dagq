# doctor and recover: what to read and when to act

Read this when diagnosing a run by hand (the `dagq-recover` skill, sections 1–3). The flags are in `"$DAGQ" doctor --help` and `"$DAGQ" recover --help`.

## doctor --full

Plain `doctor` is the compact form; diagnosing needs `--full`.

- `supervisors`: a `stale` registration is left by a killed or hung supervisor. The runtime never deletes it: report it to the person rather than removing it; `recover` works regardless of it. Empty `supervisors` means nobody owns any run.
- `runs`: each run's lease, processes and paths. `blockers` is what still prevents its recovery (`recoverable: true` when empty); only the run's own status, lease and processes count.
- `inbox_guardrail` `false`: `reference/up-down.md`, "Open the inbox again".
- `agents` `errors`: a review needing such an agent does not pass; the configuration is fixed on the landing branch.
- `ci_watch`: what `gh` resolved to on your PATH, which may differ from the supervisor's. While the supervisor cannot read the CI it claims and lands nothing.

## sccache server

`doctor` shows `sccache` only when `[run.env] RUSTC_WRAPPER` names sccache; it never starts a server. On `sccache_server_restart_failed` (`stop sccache on the host; the supervisor starts it`), show the person the server's identity, `health` and the error. On their word they stop that server on the host after checking its identity and port with `doctor`; the supervisor starts the replacement itself, which clears the attention. Do not start one by hand: it gets a foreign identity and the notice stays. A missing `sccache` field does not mean the server is healthy.

## When to recover

Leave to the supervisor:

- A `running` or `validating` run whose wrapper is alive under a stale lease: the next supervisor adopts it. Start or wait for one (`up`) and watch `show ID`.
- A run waiting for its session to exit under a live lease, or one the supervisor gave up while its session runs: the supervisor stops what is left itself.
- An `awaiting_integration` run under a stale lease, and a run whose supervisor and wrapper both died: the next supervisor takes it.
- A leaseless `validating` run whose `already_satisfied` claim rests: answer its `approve_close` ask instead.

`recover` is only for a run whose wrapper is dead or silent, a `claimed` / `starting` run, an `integrating` run (an `integrate` that died), or a run without a lease, and only while no supervisor runs. A supervisor alive without a heartbeat is stopped by the person with `down --force`.

## recover

`"$DAGQ" recover RUN_ID` makes the run `interrupted` (an `integrating` or leased `awaiting_integration` run keeps a landable status instead: land it as in `reference/review-by-hand.md`), deletes only that run's lease and keeps its worktree, branch and run directory. The next supervisor's recovery job takes an `interrupted` run.

It is refused while a process of the run is alive or its lease is fresh. With a supervisor running, leave the run to it; nothing types into a session (`run send` refuses every run). The person ends a hung supervisor or a session no supervisor serves in their own terminal; never kill processes unless the person asks.
