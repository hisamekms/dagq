#!/bin/sh
# Check the frontmatter of the documents under docs/ other than the ADRs,
# against docs/frontmatter.md:
# - the file starts with a frontmatter: a first line of "---" and a closing
#   "---";
# - the required keys id, type, title, status, created and updated are there
#   with a value, and a design document also has last_verified;
# - type is design, plan or development (adr only for the ADRs), and status
#   is one of the allowed values of that type ("Type-specific fields");
# - id is lowercase kebab-case, and no two checked documents share one.
#
# The ADRs (every .md under docs/adr/ except README.md, the template
# included) are not checked here: check-adr-numbers.sh checks their IDs,
# filenames and dates, and their required keys and status values are left to
# the review. The form of the date lines (created, updated, last_verified) is
# check-frontmatter-dates.sh's; this script only checks that the required
# ones are there.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root
# (`sh scripts/check-doc-frontmatter.sh`).
#
# Exit 0 when every frontmatter is fine, 1 when any is not (each file and the
# reason go to stderr), 2 when docs/ is not found.
set -eu

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if [ ! -d docs ]; then
  echo "check-doc-frontmatter: docs not found under $root" >&2
  exit 2
fi

bad=$(find docs -type f -name '*.md' | LC_ALL=C sort | while IFS= read -r f; do
  case $f in
    (docs/adr/README.md) ;;
    (docs/adr/*) continue ;;
  esac
  printf '%s\n' "$f"
done | awk '
  # The file names come on stdin, one per line, and each file is read with
  # getline, so a name with a space works and an empty file is reported.
  function report(msg) { out = out cur ": " msg "\n" }
  function check(cur) {
    opened = 0; closed = 0; split("", val); n = 0
    while ((r = (getline raw < cur)) > 0) {
      n++
      line = raw; sub(/\r$/, "", line)
      if (n == 1) { if (line != "---") break; opened = 1; continue }
      if (line == "---") { closed = 1; break }
      if (match(line, /^[A-Za-z_][A-Za-z0-9_]*:/)) {
        key = substr(line, 1, RLENGTH - 1)
        v = substr(line, RLENGTH + 1)
        sub(/^[ \t]+/, "", v); sub(/[ \t]+$/, "", v)
        if (v ~ /^".*"$/ || v ~ /^\047.*\047$/) v = substr(v, 2, length(v) - 2)
        val[key] = v
      }
    }
    close(cur)
    if (r < 0) { report("cannot be read"); return }
    if (!opened) { report("no frontmatter (the first line is not ---)"); return }
    if (!closed) { report("no closing --- of the frontmatter"); return }
    k = split("id type title status created updated", req, " ")
    for (i = 1; i <= k; i++)
      if (!(req[i] in val) || val[req[i]] == "") report("missing required key " req[i])
    t = val["type"]
    if ("type" in val && t != "") {
      if (t == "design") allowed = " draft current deprecated superseded "
      else if (t == "plan") allowed = " proposed active blocked completed archived "
      else if (t == "development") allowed = " current deprecated "
      else { allowed = ""; report("type " t " is not design, plan or development") }
      if (allowed != "" && ("status" in val) && val["status"] != "" && index(allowed, " " val["status"] " ") == 0)
        report("status " val["status"] " is not allowed for type " t " (allowed:" allowed ")")
      if (t == "design" && (!("last_verified" in val) || val["last_verified"] == ""))
        report("missing last_verified of a design document")
    }
    if (("id" in val) && val["id"] != "") {
      if (val["id"] !~ /^[a-z0-9]+(-[a-z0-9]+)*$/) report("id " val["id"] " is not lowercase kebab-case")
      else if (val["id"] in seen) report("id " val["id"] " is also the id of " seen[val["id"]])
      else seen[val["id"]] = cur
    }
  }
  $0 != "" { cur = $0; check(cur) }
  END { printf "%s", out }
')

if [ -n "$bad" ]; then
  echo "check-doc-frontmatter: the frontmatter of the documents other than the ADRs must follow docs/frontmatter.md:" >&2
  printf '%s\n' "$bad" | sed 's/^/  /' >&2
  exit 1
fi
