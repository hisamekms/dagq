#!/bin/sh
# A compatibility wrapper, kept only because the saved verification commands
# of registered tasks run `sh scripts/check-context-boundaries.sh`. New tasks
# must not name it. It is removed once no unfinished task's verification
# names it.
#
# It runs every test of src/context_boundaries.rs and its modules (the tests
# whose module path starts with context_boundaries::: the SQL write targets
# of C1, C7 and X1, the module references of C5 and C8, and their fixtures)
# and exits with cargo's exit code. It ignores its arguments (--self-test
# included). It reads no list and holds no rule: the
# rules and the table are in those tests and
# src/context_boundaries/ownership.rs (docs/design/architecture.md, section
# "検査の範囲").
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
exec cargo test --locked --lib context_boundaries::
