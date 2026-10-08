#!/bin/sh
# Check that no test file under tests/ (every .rs file at any depth:
# tests/*.rs, tests/common/*.rs, tests/it/*.rs and tests/it/runtime_support/*.rs)
# has more than 3,000 lines, so the files split by feature do not grow back
# into one file that every runtime task appends to and conflicts in. The
# integration tests share one test binary (tests/it, ADR-0078), but the limit
# stays per file. src/ is not checked yet.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root (`sh scripts/check-test-file-lines.sh`).
#
# Exit 0 when every file is within the limit, 1 when a file is over it (each
# offending file and its line count go to stderr), 2 when tests/ is not found.
set -eu

limit=3000

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d tests ]; then
  echo "check-test-file-lines: tests not found under $root" >&2
  exit 2
fi

status=0

for f in $(find tests -type f -name '*.rs' 2>/dev/null | sort); do
  lines=$(wc -l < "$f" | tr -d '[:space:]')
  if [ "$lines" -gt "$limit" ]; then
    echo "check-test-file-lines: $f has $lines lines, more than $limit; split it into files by feature" >&2
    status=1
  fi
done

exit "$status"
