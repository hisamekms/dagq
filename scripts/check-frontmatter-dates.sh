#!/bin/sh
# Check the date lines of the frontmatter of every .md under docs/
# (ADR-t1854-1): each updated, last_verified and created line must be the key
# and a YYYY-MM-DD date only, with no trailing comment ("# task N; ...") and
# no other value. Which task changed a document is kept by the git history.
#
# Only the frontmatter is read: from a first line of "---" to the next "---".
# A file whose first line is not "---", or with no closing "---", has no
# frontmatter and is skipped, and lines in the body (code examples and so on)
# are never checked. A CRLF line ending on a date line fails like any other
# trailing text.
#
# The one exception is docs/adr/0000-template.md, named here: its
# `created: YYYY-MM-DD` and `updated: YYYY-MM-DD` are the template's
# placeholders, not dates. Any other file with YYYY-MM-DD fails.
#
# Meant to be run from the repository root
# (`sh scripts/check-frontmatter-dates.sh`). When run from anywhere else it
# changes to the repository root found from the script's own location, so the
# result does not depend on the cwd.
#
# Exit 0 when every date line is fine, 1 when any is not (each offending file,
# line number and line go to stderr), 2 when docs/ is not found.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs ]; then
  echo "check-frontmatter-dates: docs not found under $root" >&2
  exit 2
fi

template=docs/adr/0000-template.md

bad=$(find docs -type f -name '*.md' ! -path "$template" | LC_ALL=C sort | while IFS= read -r f; do
  awk '
    { line = $0; sub(/\r$/, "", line) }
    FNR == 1 && line != "---" { exit }
    FNR > 1 && line == "---" { closed = 1; exit }
    /^(updated|last_verified|created)[[:space:]]*:/ {
      if ($0 !~ /^(updated|last_verified|created): [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) {
        out = out FILENAME ":" FNR ": " $0 "\n"
      }
    }
    END { if (closed) printf "%s", out }
  ' "$f"
done)

if [ -n "$bad" ]; then
  echo "check-frontmatter-dates: frontmatter date lines must be '<key>: YYYY-MM-DD' with no comment (ADR-t1854-1):" >&2
  printf '%s\n' "$bad" | sed 's/^/  /' >&2
  exit 1
fi
