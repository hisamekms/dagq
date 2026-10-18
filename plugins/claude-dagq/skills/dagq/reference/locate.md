# Locating the binary and the queue

Read this when `--resolve` prints an error or a warning, when `db_exists` is false, when the queue must point elsewhere, or to see where the language is set.

- An `{"error": ...}` (no binary): pass its message on. The user installs it with `cargo install --locked dagq` (`~/.cargo/bin` on PATH) or sets `DAGQ_BIN` to a `dagq` binary elsewhere; then retry.
- A `{"warning": ...}` on stderr (plugin and binary differ in major.minor): do not stop. Tell the user to update the older one (`claude plugin update claude-dagq@dagq`, or `cargo install --locked dagq`); until they match, name the mismatch as the likely cause of a failure the skill does not explain.
- The queue is one per repository, the same from every worktree: run the launcher inside the repository. `DAGQ_DB=/path/to/queue.db` uses another queue file.
- `db_exists` false: `"${CLAUDE_PLUGIN_ROOT}/bin/dagq" init` once. A command that says to run `dagq migrate` needs a person to run it with the updated binary. A queue bound to another repository is refused: unset or fix `DAGQ_DB`.
- The repository was moved or renamed (`db_exists` false though the tasks exist): do not `init`. Report it and, on the user's word: (1) `down --wait` in the old checkout and land `needs_session` runs (old checkout gone: `DAGQ_DB=<old queue.db> "${CLAUDE_PLUGIN_ROOT}/bin/dagq" down --wait`; a launchd one with `launchctl bootout gui/$(id -u)/com.dagq.<old hash>`, the old queue directory's name, and delete its plist under `~/Library/LaunchAgents`); (2) from the new checkout, `DAGQ_DB=<old queue.db> "${CLAUDE_PLUGIN_ROOT}/bin/dagq" rebind`, which prints `move_to`; (3) move the old queue directory whole to `move_to` (remove an empty one an `init` made first); (4) `up` again. Never edit the database to rebind it.

## The language

The language the `dagq` skill's section 5 has you write in is `[language] tag` of the repository's `dagq.toml` over the user's `$XDG_CONFIG_HOME/dagq/config.toml`; `"$DAGQ" doctor` shows it and where it came from.
