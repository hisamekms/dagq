# Updating the dagq binary: install, auto-update and the update asks

Read this when the person wants to update or roll back the `dagq` binary, turn on automatic updates, update to a crates.io release, or answer an `approve_release` / `update_failed` / `approve_update` ask. Everything here is typed from the inbox (or by the person in a terminal without `DAGQ_ROLE`), on the person's word; tell the person before a binary is replaced. The flags are in `"$DAGQ" install --help` and `"$DAGQ" up --help`.

## install

```sh
"$DAGQ" install --from PATH          # a binary file, or a checkout of dagq's source (built)
"$DAGQ" install                      # in dagq's source repository only: build the queue's main checkout
"$DAGQ" install --release [VERSION]  # a release from crates.io
"$DAGQ" install --rollback           # go back to <binary>.previous
```

- `install` hands every live supervisor over to the new binary; runs in flight go on. A failed handoff opens an `update_failed` ask (below).
- Never replace the file with `cp`: on macOS that can kill the processes running it, and it leaves no `.previous`.
- A build of dagq's source runs the e2e suite first and replaces nothing when it fails. `--skip-e2e` only on the person's explicit word, when it cannot wait.
- A supervisor the output says was not handed over is replaced by the next `up`. When `install` refuses because the new binary predates the handoff: `down --wait`, the same `install`, then `up`.
- **`--release`** does not update the plugin: after it, run `claude plugin marketplace update dagq` and `claude plugin update claude-dagq@dagq`, then the person reopens the inbox and planner sessions (`up-down.md`, "Open the inbox again").

### Breaking migrations

A breaking migration requires `install --allow-breaking`. The `install` answer to an `approve_update` ask uses that same command. The stop drain, also used by `down` and `down --wait`, finishes runs that can proceed. The next supervisor takes over the wait record and lease only for slot runs left `running` in a recorded answer wait blocked by a hold or that run's unanswered ask, or a recorded return wait blocked by a hold. An open ask alone is insufficient for this handover.

Before running the install command, show the person the asks and holds blocking those waits. Answer in their words or release holds as they direct so the runs can proceed. Check `status` to confirm those runs have left the in-use statuses before running or retrying install.

After the drain, install checks for remaining queue users while the old service and broker are still up. The run's status decides whether it blocks migration, regardless of its lease. A run in `claimed`, `starting`, `running`, `validating` or `integrating` (including the preserved waits), a live wrapper or a live supervisor causes refusal before migration is attempted. The error names the remaining users. A lease-free `awaiting_integration` run pending landing approval falls outside the handover and does not block migration by itself, even with an open ask.

An unreadable queue or a failure to stop the old services also prevents migration. If the check refuses or migration fails before replacement, install reuses the old service if it answers. Otherwise it tries to start the service with the old binary. With a usable service and a drained supervisor registration, it tries to restart that supervisor with the same arguments. A failed service recovery skips supervisor startup. The error reports the original reason and both recovery results. Read any reported recovery failure and use it to recover.

Once the check passes, install stops the old services before migration and restarts the drained supervisor after a successful replacement. Pass what that `up` needs (`--plugin-dir` only when the repository's instructions name one, never `$CLAUDE_PLUGIN_ROOT`). A `--rollback` past a breaking migration is refused and the error names the backup.

## An inbox refused as `unauthenticated` after an update

The person opens the inbox again from a terminal without `DAGQ_ROLE` (`up-down.md`, "Open the inbox again"). A runtime planner refused the same way: wait for it to close.

## Automatic updates (`up --auto-update`)

```sh
"$DAGQ" up --auto-update   # with the same other flags as always
```

It needs no restart. An `up` without the flag turns it off again, so every later `up` (the `restart supervisor` one too) must carry it. A success reaches the inbox as `report the update`; a failure opens `update_failed`.

## Release updates (`approve_release`)

A supervisor on a release build asks about a new release as the host's settings choose, and applies the answer: `install` installs the release and updates the plugin; `skip` does not ask about that version again. When the plugin was updated, tell the person to reopen the inbox and planner sessions (`up-down.md`, "Open the inbox again"). A plugin-only ask (its question starts `The claude-dagq plugin installed in Claude Code is ...`) touches only the plugin.

With no supervisor that applies it, the answer comes back as `read the answer of ask <id> and close it`: on `install` and the person's word, `"$DAGQ" install --release <version>` and the two plugin commands (only those for a plugin-only ask); then `ask close <id>`. On `skip`, `ask close <id>`.

A release with a breaking migration is never installed by itself: it opens `approve_update` (below). Do not type `up --auto-update` after its drain.

## The update asks

Show each question as written. An `update_failed` the runtime closed itself needs nothing, not even telling the person.

- **`update_failed`** (`retry` / `skip`): If the question contains both `the previous build was left unrestored` and `does not accept the update job's principal`, on the person's word run `install --rollback`, then `ask close <id>`. Otherwise, answer with the person's option. A live `--auto-update` supervisor applies it. When it comes back as `read the answer of ask <id> and close it`, nothing applied it: update by hand with `install` if the person wants, then `ask close <id>`. If no supervisor serves the queue after it, `up` starts one with the binary in place.
- **`update_failed` of a person's `install`** (its question starts with A person's `dagq install`): no supervisor applies it. On the person's word do what they chose from the question: the same `install` after the fix, `install --rollback`, or have the person type `dagq down --force` in their own terminal without `DAGQ_ROLE` and confirm its target on the TTY, then `up`; or leave it. Then `ask close <id>`.
- **`update_failed` of a release job** (its question names the release): a release supervisor applies it as for `approve_release`; with none it comes back to be done by hand: `"$DAGQ" install --release <version>` (or `install --from <binary>` for a binary installed another way), then `ask close <id>`. A `build` failure is read in the cargo log the question names (an old Rust, no C compiler, no network); the person fixes it, then `retry`. A `plugin` failure left the binary as the question says: fix what it shows, then `retry` or by hand run the two plugin commands and `ask close <id>`.
- **`approve_update`** (`install` / `skip`): a build with a breaking migration, which the runtime never applies itself. On `install` and the person's word, follow [Breaking migrations](#breaking-migrations) and run the question's `install --allow-breaking` command as written. After a successful install, run `ask close <id>`. On `skip`, run `ask close <id>`.
