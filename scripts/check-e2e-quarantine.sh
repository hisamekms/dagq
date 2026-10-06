#!/bin/sh
# Check .config/e2e-quarantine.toml, the marks of the e2e gate
# (ADR-t1165-1; format in docs/design/supervisor-lifecycle/auto-update.md).
# The gate reads the file itself and holds no mark of a file it cannot read,
# so a broken file silently turns every mark off; this catches it on the
# commit instead.
#
# Checked: the file is made of [[test]] tables, each with exactly one name,
# reason, task (a positive integer) and until (a date YYYY-MM-DD, bare or
# quoted), and no other key or table; no name is marked twice; each name is
# a test the e2e binary has (a top-level name is a #[test] fn in
# tests/e2e.rs, and `<module>::…::<fn>` one in tests/e2e/<module>.rs or the
# module's file under tests/e2e/<module>/); and there are at most 3 marks (the
# gate's e2e_quarantine::LIMIT). A mark whose until is past only warns: the
# gate simply does not hold it.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (the program review of a run;
# docs/development/task-registration.md, section "推奨の組み合わせ").
# Outside a git work tree it is the repository found from the script's own
# location. Run it from the repository root (`sh scripts/check-e2e-quarantine.sh`).
# E2E_QUARANTINE_FILE names another file to check (relative to that tree's
# root or absolute).
#
# Exit 0 when the file is absent or valid, 1 when it has a violation (each
# goes to stderr with the test's name or the line), 2 when tests/e2e.rs is
# not found.
set -eu

limit=3

root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

file=${E2E_QUARANTINE_FILE:-.config/e2e-quarantine.toml}
me=check-e2e-quarantine

if [ ! -f tests/e2e.rs ]; then
  echo "$me: tests/e2e.rs not found under $root" >&2
  exit 2
fi

if [ ! -e "$file" ]; then
  echo "$me: $file is absent; no marks"
  exit 0
fi

# awk prints one line per finding: "error<TAB>message" or
# "mark<TAB>name<TAB>until" for each complete mark.
parsed=$(LC_ALL=C awk '
function err(msg) { gsub(/\n/, "\\\\n", msg); gsub(/\t/, "\\\\t", msg); print "error\t" msg }
function strip_comment(s,   i) {
  i = index(s, "#"); if (i > 0) s = substr(s, 1, i - 1)
  sub(/^[ \t]+/, "", s); sub(/[ \t]+$/, "", s); return s
}
# Read a quoted string at the start of s; sets STR and returns 1, or sets
# WHY and returns 0.
function qstring(s,   q, i, c, out, rest) {
  q = substr(s, 1, 1)
  if (q != "\"" && q != "\047") { WHY = "expected a quoted string, not " s; return 0 }
  out = ""
  for (i = 2; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == q) {
      rest = strip_comment(substr(s, i + 1))
      if (rest != "") { WHY = "unexpected text after the string: " rest; return 0 }
      STR = out; return 1
    }
    if (c == "\\" && q == "\"") {
      i++; c = substr(s, i, 1)
      if (c == "\"" || c == "\\") out = out c
      else if (c == "n") out = out "\n"
      else if (c == "t") out = out "\t"
      else { WHY = "unknown escape \\" c; return 0 }
      continue
    }
    out = out c
  }
  WHY = "the string " s " is not closed"; return 0
}
function valid_date(d,   y, m, day, days) {
  if (d !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) return 0
  y = substr(d, 1, 4) + 0; m = substr(d, 6, 2) + 0; day = substr(d, 9, 2) + 0
  if (m < 1 || m > 12 || day < 1) return 0
  days = 31
  if (m == 4 || m == 6 || m == 9 || m == 11) days = 30
  if (m == 2) days = (y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)) ? 29 : 28
  return day <= days
}
function finish() {
  if (!open) return
  if ("name" in has) label = has["name"]; else label = "the [[test]] of line " start
  if (!("name" in has)) err("the [[test]] of line " start " has no name")
  if (!("reason" in has)) err(label ": no reason")
  if (!("task" in has)) err(label ": no task")
  if (!("until" in has)) err(label ": no until")
  if (("name" in has) && ("until" in has)) print "mark\t" has["name"] "\t" has["until"]
  open = 0
}
{
  sub(/\r$/, "")
  if (NR == 1) sub(/^\357\273\277/, "")
  line = $0; sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") next
  if (substr(line, 1, 1) == "[") {
    header = strip_comment(line)
    if (header != "[[test]]") { err("line " NR ": only [[test]] tables are known, not " header); next }
    finish()
    open = 1; start = NR; delete has
    next
  }
  if (!open) { err("line " NR ": a key outside a [[test]] table"); next }
  eq = index(line, "=")
  if (eq == 0) { err("line " NR ": expected KEY = value"); next }
  key = substr(line, 1, eq - 1); sub(/[ \t]+$/, "", key)
  value = substr(line, eq + 1); sub(/^[ \t]+/, "", value)
  if (key in has) { err("line " NR ": " key " is given twice"); next }
  if (key == "name" || key == "reason") {
    if (!qstring(value)) { err("line " NR ": " key ": " WHY); next }
    if (STR == "") { err("line " NR ": " key ": is empty"); next }
    if (key == "name" && (STR !~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$/)) {
      err("line " NR ": name: " STR " is not a test name (letters, digits, _ and ::)"); next
    }
    has[key] = STR
  } else if (key == "task") {
    v = strip_comment(value)
    if (v !~ /^[0-9]+$/ || length(v) > 18 || v + 0 <= 0) { err("line " NR ": task: expected a task ID, not " value); next }
    has[key] = v
  } else if (key == "until") {
    v = strip_comment(value)
    if (v ~ /^["\047]/) { if (!qstring(value)) { err("line " NR ": until: " WHY); next } v = STR }
    if (!valid_date(v)) { err("line " NR ": until: expected a date YYYY-MM-DD, not " v); next }
    has[key] = v
  } else {
    err("line " NR ": unknown key " key); next
  }
}
END { finish() }
' "$file")

status=0
today=$(date +%Y%m%d)
tab=$(printf '\t')
marks=0
seen=' '

# Whether file $2 has a #[test] fn called $1: the fn line follows #[test]
# with only attributes and comments between.
fn_is_test() {
  LC_ALL=C awk -v fn="$1" '
    { line = $0; sub(/^[ \t]+/, "", line) }
    line ~ /^#\[test\]/ { pending = 1; next }
    pending && (line ~ /^#\[/ || line ~ /^\/\//) { next }
    pending && line ~ ("^(pub[ \t]+)?(async[ \t]+)?fn[ \t]+" fn "[ \t]*[(<]") { found = 1; exit }
    { pending = 0 }
    END { exit found ? 0 : 1 }
  ' "$2"
}

# Whether the e2e binary has a test fn called $1 (cargo's name): a
# top-level name in tests/e2e.rs, `<module>::…::<fn>` in the module's file
# under tests/e2e/ (or an inline module of tests/e2e/<module>.rs).
has_test() {
  name=$1
  fn=${name##*::}
  case $name in
  *::*)
    module=${name%::*}
    top=${module%%::*}
    dir=tests/e2e/$(printf '%s' "$module" | sed 's|::|/|g')
    for f in "tests/e2e/$top.rs" "$dir.rs" "$dir/mod.rs"; do
      if [ -f "$f" ] && fn_is_test "$fn" "$f"; then
        return 0
      fi
    done
    return 1
    ;;
  *)
    fn_is_test "$fn" tests/e2e.rs
    ;;
  esac
}

while IFS="$tab" read -r kind first second; do
  [ -n "$kind" ] || continue
  case $kind in
  error)
    echo "$me: $file: $first" >&2
    status=1
    ;;
  mark)
    marks=$((marks + 1))
    case $seen in
    *" $first "*)
      echo "$me: $file: $first is marked twice" >&2
      status=1
      ;;
    *) seen="$seen$first " ;;
    esac
    if ! has_test "$first"; then
      echo "$me: $file: $first is not a test in tests/e2e.rs or tests/e2e/" >&2
      status=1
    fi
    if [ "$(printf '%s' "$second" | tr -d -)" -lt "$today" ]; then
      echo "$me: warning: $file: the mark of $first expired after $second; the gate does not hold it (remove it or give it a new until)" >&2
    fi
    ;;
  esac
done <<EOF
$parsed
EOF

if [ "$marks" -gt "$limit" ]; then
  echo "$me: $file has $marks marks, more than $limit; the gate holds none of them" >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "$me: $file: $marks marks, at most $limit"
fi

exit "$status"
