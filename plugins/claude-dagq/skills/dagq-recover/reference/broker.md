# When the resource broker cannot be used

What the modes mean and how to read `status`, `doctor` and `dagq broker status`: `${CLAUDE_PLUGIN_ROOT}/skills/dagq/reference/broker.md`. Everything here is for a queue whose `[broker] mode` is `preferred` or `required`; with `disabled` the supervisor never calls podman and none of this shows. Diagnose first, changing nothing:

```sh
"$DAGQ" broker status     # asks podman and the broker; changes nothing
"$DAGQ" broker logs       # the container's log tail (fails with machine_missing / machine_stopped / container_missing instead of making one)
"$DAGQ" doctor --full     # broker.podman, broker.client, broker.health
```

## The two attentions

Both have `next: dagq broker status`, no run or task, and `status` set to a code (the table below). Neither is closed by hand: each clears by itself.

- **`broker_unhealthy`** (mode `preferred` or `required`): the supervisor could not make the broker ready, or its health failed again after the automatic restart (that restart itself is an `auto_repaired` with `repair: broker_restart`). It tries again every 30 seconds, from the machine up, and the attention clears on `broker_started` or `broker_healthy`. Under `preferred`, claims and landings go on and the runs get no broker tools meanwhile (`broker_unavailable` on each run).
- **`broker_claims_held`** (mode `required` only): no worker can be given the broker's tools now, so the supervisor claims and resumes nothing. Landings, reviews and the runs in flight go on. It never runs a worker with the built-in tools instead. It looks again every pass and clears on `broker_claims_resumed`. When the broker itself is down, `broker_unhealthy` stands beside it.

## What each code asks of the person

| `status` | what it means | what the person does |
| --- | --- | --- |
| `not_ready` | the broker is still being made ready (the first image build takes minutes) | wait; it clears by itself |
| `podman_missing` | podman is not found (`host.toml`'s `[broker] podman`, else the PATH) | install it (`brew install podman`); `up` refuses to start a supervisor while it is missing |
| `machine_busy` | another Podman machine runs, and dagq's machine `dagq` cannot start beside it | dagq never stops another machine: the person stops their own (`podman machine stop <name>`) or waits until it is done |
| `machine_failed` | `dagq`'s `podman machine init` / `start` failed, even after one stop and start | read the message; then, on the person's word, `"$DAGQ" broker start` tries again by hand (it inits and starts only `dagq`). Never `podman machine init` / `rm` it by hand, and never touch the person's default machine |
| `image_build_failed`, `image_source_missing` | the image could not be built (the machine needs network to the package and image registries), or the dagq binary has no image material | fix the network and `"$DAGQ" broker start`; for the material, install the binary again (`reference/update.md`) |
| `container_failed`, `unhealthy` | the container did not start, or its health stopped answering | `"$DAGQ" broker logs`; on the person's word restart it (below) |
| `version_mismatch`, `client_missing` | the broker's or the client's build is not the dagq binary's (after an update, or a client missing beside `dagq`) | a broker of another build is kept while runs hold tokens on it and is replaced once they end; a client that is missing or of another build never clears by waiting: install again so both are in place (`reference/update.md`) |
| `token_failed` (`broker_claims_held`) | no run's token can be issued: the signing key or the token dirs cannot be made, or the main checkout has no `git config user.name` / `user.email` | set the committer in the main checkout, or fix the queue dir's permissions; it clears next pass |
| `grant_failed` (`broker_claims_held`) | one claimed run could not be given the tools (its branch names no committer, a Codex or interactive worker) | that run fails as below; other claims go on |

## Restart the broker

Only on the person's word, from a terminal without `DAGQ_ROLE` or the inbox:

```sh
"$DAGQ" broker stop    # removes the queue's container, then stops dagq's machine when no other container runs on it
"$DAGQ" broker start   # idempotent: machine, image, container, health
```

`broker stop` does not wait for the runs: a running worker's broker tools fail until `start` answers, so prefer it when `active_tokens` is 0, or tell the person which runs it hits. A supervisor that sees the broker gone makes it ready again by itself, so `start` is only to try at once. `down --wait` also stops the broker once no supervisor of the queue is left.

To stop using the broker on this host for now, the person may set `[broker] mode = "disabled"` in the queue's or the host's `host.toml` and start the supervisor again (`up`, `SKILL.md` section 5). Under `required` that sends the workers back to the built-in tools, so it is the person's decision, never the inbox's.

## A run the broker failed

- Under `required`, a worker whose broker tool returned a structured error (`unauthorized`, `transport`, `config`, `protocol`: no broker, no token) writes a `failed` receipt (or asks) instead of finding another way; its summary names the error. Under `preferred` the worker falls back to the built-in tools instead, so such a run rarely fails on the broker. The supervisor never stops a run because the broker went down.
- A `required` run whose tools could not be given after its claim ends `failed` (`runtime_error`, `broker_required: ...`) without opening a workspace; a resume refused that way is given up (`resume_finished` with that error), a reopen fails with `session_reopen_failed`.
- All of them go to the recovery job like any failed run. The cause is the host, not the task: retry it only once `dagq broker status` shows `running` with `build_matches: true`. When the job escalates (`decide`) or the triage failed (`triage by hand`), tell the person the broker error and offer the retry, `"$DAGQ" ready ID` (a new run; `reference/triage-by-hand.md`). A broker error alone is no reason to cancel the task.
