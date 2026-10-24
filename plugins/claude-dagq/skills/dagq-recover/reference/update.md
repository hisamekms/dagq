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
- **A breaking migration**: `install` refuses unless `--allow-breaking`, which drains (show the person the open asks first) and starts the supervisor again. Pass what that `up` needs (`--plugin-dir` only when the repository's instructions name one, never `$CLAUDE_PLUGIN_ROOT`). A `--rollback` past a breaking migration is refused; the error names the backup.
- **`--release`** does not update the plugin: after it, run `claude plugin marketplace update dagq` and `claude plugin update claude-dagq@dagq`, then the person reopens the inbox and planner sessions (`up-down.md`, "Open the inbox again").

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

- **`update_failed`** (`retry` / `skip`): answer with the person's option. A live `--auto-update` supervisor applies it. When it comes back as `read the answer of ask <id> and close it`, nothing applied it: update by hand with `install` if the person wants, then `ask close <id>`. If no supervisor serves the queue after it, `up` starts one with the binary in place.
- **`update_failed` of a person's `install`** (its question starts with A person's `dagq install`): no supervisor applies it. On the person's word do what they chose from the question: the same `install` after the fix, `install --rollback`, or `down --force` and `up`; or leave it. Then `ask close <id>`.
- **`update_failed` of a release job** (its question names the release): a release supervisor applies it as for `approve_release`; with none it comes back to be done by hand: `"$DAGQ" install --release <version>` (or `install --from <binary>` for a binary installed another way), then `ask close <id>`. A `build` failure is read in the cargo log the question names (an old Rust, no C compiler, no network); the person fixes it, then `retry`. A `plugin` failure left the binary as the question says: fix what it shows, then `retry` or by hand run the two plugin commands and `ask close <id>`.
- **`approve_update`** (`install` / `skip`): a build with a breaking migration, which the runtime never applies itself. On `install` and the person's word, run the command from the question as written (it drains as `--allow-breaking` does), then `ask close <id>`. On `skip`, `ask close <id>`.
