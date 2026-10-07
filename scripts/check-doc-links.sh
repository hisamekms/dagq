#!/bin/sh
# Check the relative links of the Markdown documents: every .md under docs/
# and every .md at the repository root. Each inline link or image
# `[text](target)` or `![alt](target)` whose target is a relative path must
# name a file or a directory that exists in the repository, resolved from the
# document's directory (a target starting with "/" from the repository root).
#
# What is not checked:
# - external targets (a scheme such as https:, http:, mailto:) and targets
#   that are only an anchor ("#section");
# - the anchor part of "path#anchor" (only the path part is checked), and a
#   "?query" part;
# - links inside fenced code blocks (``` or ~~~) and inline code spans
#   (`...`), which are examples, not links;
# - reference-style links ("[text][ref]" with "[ref]: target"), and
#   autolinks ("<https://...>").
# Known limits, kept for a short script: indented code blocks (4 spaces) are
# read as text, so a broken link in one fails; a target is cut at its first
# ")" (so "[t](a_(b).md)" reads "a_(b"); and a target with ".." is not kept
# inside the repository.
# A target in angle brackets ("[t](<a b.md>)") is read without them, and an
# optional title ("[t](a.md "title")") is dropped. Percent-encoded spaces
# ("%20") are decoded.
#
# The one exception: the links of the ADRs (docs/adr/) into docs/journal/,
# which was removed after it was frozen (ADR-0008 links two journal entries).
# The ADRs are append-only (docs/development/documents.md, section "ADR"), so
# those links are not rewritten and are skipped here; a link of any other
# document into docs/journal/ fails like any other.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root (`sh scripts/check-doc-links.sh`).
#
# Exit 0 when every relative link resolves, 1 when any does not (each file,
# line number and target go to stderr), 2 when docs/ is not found.
set -eu

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs ]; then
  echo "check-doc-links: docs not found under $root" >&2
  exit 2
fi

# One "file<TAB>line<TAB>target<TAB>path" per relative link, where path is
# the target joined to the document's directory.
links=$({
  find docs -type f -name '*.md'
  find . -maxdepth 1 -type f -name '*.md' | sed 's|^\./||'
} | LC_ALL=C sort | while IFS= read -r f; do
  awk -v file="$f" '
    BEGIN { dir = file; if (!sub(/\/[^\/]*$/, "", dir)) dir = "." }
    {
      line = $0
      sub(/\r$/, "", line)
      # A fence opens with 3 or more ` or ~ and closes with the same
      # character, at least as many, and nothing after them.
      if (match(line, /^ ? ? ?(```+|~~~+)/)) {
        f = substr(line, 1, RLENGTH)
        sub(/^ */, "", f)
        rest = substr(line, RLENGTH + 1)
        if (infence == "") { infence = f; next }
        if (substr(f, 1, 1) == substr(infence, 1, 1) && length(f) >= length(infence) && rest ~ /^[ \t]*$/) {
          infence = ""
          next
        }
      }
      if (infence != "") next
      # Drop inline code spans.
      gsub(/`[^`]*`/, "", line)
      while (match(line, /\]\([^)]*\)/)) {
        target = substr(line, RSTART + 2, RLENGTH - 3)
        line = substr(line, RSTART + RLENGTH)
        sub(/^[ \t]+/, "", target)
        if (target ~ /^</) {
          sub(/^</, "", target)
          sub(/>.*$/, "", target)
        } else {
          sub(/[ \t].*$/, "", target)
        }
        if (target == "" || target ~ /^#/ || target ~ /^[A-Za-z][A-Za-z0-9+.-]*:/) continue
        sub(/#.*$/, "", target)
        sub(/\?.*$/, "", target)
        gsub(/%20/, " ", target)
        if (target == "") continue
        if (target ~ /^\//) path = "." target
        else path = dir "/" target
        # The ADRs are append-only, so their links to the removed
        # docs/journal/ stay (see the header).
        if (file ~ /^docs\/adr\// && path ~ /^docs\/adr\/\.\.\/journal\//) continue
        printf "%s\t%d\t%s\t%s\n", file, FNR, target, path
      }
    }
  ' "$f"
done)

bad=$(printf '%s\n' "$links" | while IFS='	' read -r f n target path; do
  [ -n "$f" ] || continue
  [ -e "$path" ] || printf '%s:%s: %s\n' "$f" "$n" "$target"
done)

if [ -n "$bad" ]; then
  echo "check-doc-links: relative links must name a file or directory in the repository:" >&2
  printf '%s\n' "$bad" | sed 's/^/  /' >&2
  exit 1
fi
