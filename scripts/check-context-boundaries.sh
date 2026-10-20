#!/bin/sh
# The rules this script checked, C5 and the records of the ports and of the
# modules of src/application/ that C7 holds, are checked by the Rust test of
# the module references (src/context_boundaries/modules.rs and its ports
# module), with C1, C7 and X1 in src/context_boundaries.rs; their table is
# src/context_boundaries/ownership.rs and what they check is in
# docs/design/architecture.md, section "検査の範囲". `cargo test` runs them.
#
# The script checks nothing and reads no list (.config/context-boundaries.txt
# and .config/context-boundaries-allow.txt stay unread, for an older copy of
# this script the program review may run): it stays only until it is
# retired with scripts/check-layer-deps.sh (ADR-t2397-1 decision 4), so
# that what already runs it (the verify of registered tasks, CI and the
# program review of dagq.toml) still passes.
#
# Exit 0, with --self-test too.
set -eu

me=check-context-boundaries
if [ "${1:-}" = "--self-test" ]; then
  echo "$me --self-test: ok (no rule left to test)"
  exit 0
fi
echo "$me: ok (the rules are the Rust test src/context_boundaries/modules.rs)"
