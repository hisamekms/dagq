#!/bin/sh
# Check that AGENTS.md stays within its size limit in bytes (not lines: a
# Japanese line can be long, so the line count does not show the size). Every
# session reads AGENTS.md, so it holds only the short guide, and the rules
# themselves go to their canonical documents (docs/development/ and others,
# ADR-t1453-2). The limit and how it was chosen are recorded in
# docs/development/documents.md, section "AGENTS.md".
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root (`sh scripts/check-agents-md-size.sh`).
#
# Exit 0 when AGENTS.md is within the limit, 1 when it is over it (its size and
# the limit go to stderr), 2 when AGENTS.md is not found.
set -eu

limit=9216

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
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
