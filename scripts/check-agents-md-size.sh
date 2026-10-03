#!/bin/sh
# Check that AGENTS.md stays within its size limit in bytes (not lines: a
# Japanese line can be long, so the line count does not show the size). Every
# session reads AGENTS.md, so it holds only the short guide, and the rules
# themselves go to their canonical documents (docs/development/ and others,
# ADR-t1453-2). The limit and how it was chosen are recorded in
# docs/development/documents.md, section "AGENTS.md".
#
# Meant to be run from the repository root (`sh scripts/check-agents-md-size.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location, so the result does not depend on the cwd.
#
# Exit 0 when AGENTS.md is within the limit, 1 when it is over it (its size and
# the limit go to stderr), 2 when AGENTS.md is not found.
set -eu

limit=9216

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -f AGENTS.md ]; then
  echo "check-agents-md-size: AGENTS.md not found under $root" >&2
  exit 2
fi

bytes=$(wc -c < AGENTS.md | tr -d '[:space:]')
if [ "$bytes" -gt "$limit" ]; then
  echo "check-agents-md-size: AGENTS.md has $bytes bytes, more than the limit of $limit bytes; write the rule in its canonical document (docs/development/documents.md, section AGENTS.md) and leave only a pointer here" >&2
  exit 1
fi
