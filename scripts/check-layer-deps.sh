#!/bin/sh
# A compatibility wrapper, kept only because the saved verification commands
# of registered tasks run `sh scripts/check-layer-deps.sh`. New tasks must
# not name it. It is removed once no unfinished task's verification names it.
#
# It runs the Rust test of the module references on this tree
# (src/context_boundaries/modules.rs, whose check includes the ports of
# modules/ports.rs) and the fixtures of its layer rules (module_fixtures.rs):
# the tests whose module path starts with context_boundaries::module. It
# ignores its arguments (--self-test included) and exits with cargo's exit
# code. It reads no list and holds no rule: the rules and the table are in
# those tests and src/context_boundaries/ownership.rs
# (docs/design/architecture.md, section "検査の範囲").
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
exec cargo test --locked --lib context_boundaries::module
