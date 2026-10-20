#!/bin/sh
# The rules this script checked (L2's part clippy cannot hold and C8, before
# them L1, L3, L6, L8 and L9) are checked by the Rust test of the module
# references (src/context_boundaries/modules.rs and its ports module), with
# the table src/context_boundaries/ownership.rs; what they check is in
# docs/design/architecture.md, section "検査の範囲". `cargo test` runs them.
#
# The script checks nothing and reads no list: it stays only until it is
# retired with scripts/check-context-boundaries.sh (ADR-t2397-1 decision 4),
# so that what already runs it (the verify of registered tasks and CI) still
# passes.
#
# Exit 0, with --self-test too.
set -eu

me=check-layer-deps
if [ "${1:-}" = "--self-test" ]; then
  echo "$me --self-test: ok (no rule left to test)"
  exit 0
fi
echo "$me: ok (the rules are the Rust test src/context_boundaries/modules.rs)"
