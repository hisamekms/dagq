#!/bin/sh
# The budget of the integration tests (dagq::it, the binary of tests/it):
# the number of its tests, their summed seconds and each test's seconds
# recorded by one cargo nextest JUnit file, against .config/it-budget.toml. The
# nightly workflow .github/workflows/it-budget.yml runs it on main; the rule
# and how an over-budget night becomes a fix is in docs/development/
# testing.md, section "CI". It is not a gate of the landing nor of ci.yml.
#
# Usage: sh scripts/check-it-budget.sh [--budget FILE] [--allow FILE] [--report FILE] [JUNIT]
#        sh scripts/check-it-budget.sh --self-test
#   --budget FILE  the budget (default: .config/it-budget.toml of the git
#                  work tree of the cwd, or of the script's repository when
#                  the cwd is in none)
#   --allow FILE   read this allow file alone instead of the repository's
#                  .config/it-slow-allow.toml and .config/it-slow-allow.d/*.toml
#   --report FILE  also write the report (Markdown) to FILE
#   JUNIT          the JUnit file of a cargo nextest run (a profile with
#                  [profile.<name>.junit], e.g. NEXTEST_PROFILE=ci). Without
#                  it, only the budget file is checked.
#   --self-test    check the script on the fixtures under
#                  scripts/check-it-budget-fixtures/ and exit
#
# The budget file has one key per line, `key = value`, with # comments:
#   count   = <the most dagq::it tests a run may have, an integer>
#   secs    = <the largest sum of the dagq::it tests' seconds, a number>
#   test_secs = <the most seconds of one test without an allow-list item>
#   decided = "<YYYY-MM-DD, the day the values were set>"
#   basis   = "<what the values were set from>"
# All five keys, each once, and no other key. A file that breaks this,
# including an older budget without test_secs, is malformed (exit 1).
#
# The JUnit: every <testcase> inside the <testsuite name="dagq::it"> counts
# once, with the seconds of its time attribute (nextest writes one testcase
# per test, the time of the attempt it reports; #[ignore]d tests and tests
# not built for the platform are not in the file). The module of a test is
# the first segment of its name, the file or directory under tests/it. The
# report names the count, the seconds, the budget, what is over it and the
# 10 modules with the most seconds, plus every test over test_secs without
# an allow-list item, with its full name and seconds. Allow-list format and
# full-name matching are the same as check-it-test-time.sh: module::test
# (including nested modules), never the function name alone. Allowed tests
# still count towards the total count and seconds.
#
# Exit status: 0 when the run is within the budget (or, without JUNIT, the
# budget file is well-formed), 1 when the count or the seconds is over the
# budget, an unlisted test exceeds test_secs, or the budget is malformed;
# 2 on a usage error, an unreadable or malformed allow list, a missing
# budget file, a report that cannot be written, or a JUnit that is missing,
# unreadable, cut off (no </testsuites>) or without a dagq::it testsuite with
# a test in it. A value equal to the budget is not over it (the seconds in
# whole milliseconds). Only sh, a POSIX awk and git are needed.
set -eu

me=check-it-budget
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "usage: sh scripts/check-it-budget.sh [--budget FILE] [--allow FILE] [--report FILE] [JUNIT] | --self-test" >&2
  exit 2
}

# Prints "count secs test_secs decided basis" checked, or the problems on stderr and
# exits 1.
read_budget() {
  awk -v me="$me" -v file="$1" '
    function bad(msg) { print me ": " file ":" NR ": " msg > "/dev/stderr"; err = 1 }
    /^[ \t]*(#|$)/ { next }
    {
      line = $0
      sub(/[ \t]+#[^"]*$/, "", line)
      if (match(line, /^[ \t]*[a-z_]+[ \t]*=[ \t]*/) == 0) { bad("not a key = value line"); next }
      key = substr(line, RSTART, RLENGTH); sub(/[ \t]*=.*/, "", key); sub(/^[ \t]*/, "", key)
      val = substr(line, RLENGTH + 1); sub(/[ \t]+$/, "", val)
      if (key in seen) { bad("key " key " twice"); next }
      seen[key] = 1
      if (key == "count") {
        if (val !~ /^[0-9]+$/) bad("count is not an integer: " val); else count = val
      } else if (key == "secs") {
        if (val !~ /^[0-9]+(\.[0-9]+)?$/) bad("secs is not a number: " val); else secs = val
      } else if (key == "test_secs") {
        if (val !~ /^[0-9]+(\.[0-9]+)?$/) bad("test_secs is not a number: " val); else test_secs = val
      } else if (key == "decided") {
        if (val !~ /^"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"$/) bad("decided is not a quoted YYYY-MM-DD: " val)
        else decided = substr(val, 2, 10)
      } else if (key == "basis") {
        if (val !~ /^".+"$/) bad("basis is not a quoted string: " val); else basis = substr(val, 2, length(val) - 2)
      } else bad("unknown key " key)
    }
    END {
      n = split("count secs test_secs decided basis", keys, " ")
      for (i = 1; i <= n; i++) if (!(keys[i] in seen)) { print me ": " file ": no " keys[i] > "/dev/stderr"; err = 1 }
      if (err) exit 1
      printf "%s\t%s\t%s\t%s\t%s\n", count, secs, test_secs, decided, basis
    }' "$1"
}

# Prints "test<TAB>secs" per dagq::it test, or exits 2 with the reason.
read_junit() {
  awk -v me="$me" -v file="$1" '
    /<testsuite[ >]/ {
      insuite = ($0 ~ /<testsuite [^>]*name="dagq::it"/)
      if (insuite) found = 1
      if ($0 ~ /<testsuite [^>]*\/>/) insuite = 0
    }
    insuite && /<testcase[ >]/ {
      if (match($0, / name="[^"]*"/) == 0) { print me ": " file ":" NR ": a testcase without a name" > "/dev/stderr"; bad = 1; next }
      name = substr($0, RSTART + 7, RLENGTH - 8)
      if (match($0, / time="[0-9.]+"/) == 0) { print me ": " file ":" NR ": " name " has no time" > "/dev/stderr"; bad = 1; next }
      printf "%s\t%s\n", name, substr($0, RSTART + 7, RLENGTH - 8)
      n++
    }
    /<\/testsuite>/ { insuite = 0 }
    /<\/testsuites>/ { closed = 1 }
    END {
      if (bad) exit 2
      if (!closed) { print me ": " file ": no </testsuites> (cut off)" > "/dev/stderr"; exit 2 }
      if (!found || n == 0) { print me ": " file ": no dagq::it testsuite with a test in it" > "/dev/stderr"; exit 2 }
    }' "$1"
}

# Read the same [[test]] items as check-it-test-time.sh, validating the
# union (including duplicate names across files) before granting exemptions.
read_allow() {
  for f in "$@"; do
    if [ ! -f "$f" ] || [ ! -r "$f" ]; then
      echo "$me: cannot read the allow list $f" >&2; return 2
    fi
  done
  LC_ALL=C awk -v me="$me" '
function fail_at(where, msg) { printf "%s: %s: %s\n", me, where, msg > "/dev/stderr"; bad = 1 }
function fail_allow(msg) { fail_at(FILENAME ":" FNR, msg) }
function close_item() {
  if (!initem) return
  if (iname == "") fail_at(iwhere, "an item has no name")
  else if (ireason == "") fail_at(iwhere, "item " iname " has no reason")
  else if (itask == "") fail_at(iwhere, "item " iname " has no task")
  else if (iname in allowed) fail_at(iwhere, "item " iname " is listed twice (first at " allowed[iname] ")")
  else { allowed[iname] = iwhere; anames[++na] = iname }
  initem = 0
}
FNR == 1 { close_item() }
function strval(v) {
  if (v !~ /^"([^"\\]|\\.)*"$/) return "\001"
  return substr(v, 2, length(v) - 2)
}
{
  line = $0
  sub(/^[ \t]+/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") next
  if (line ~ /^\[\[test\]\][ \t]*(#.*)?$/) { close_item(); initem = 1; iwhere = FILENAME ":" FNR; iname = ""; ireason = ""; itask = ""; split("", seenkey); next }
  if (!initem) { fail_allow("a key outside a [[test]] item"); next }
  key = line; sub(/[ \t]*=.*/, "", key)
  val = line; sub(/^[^=]*=[ \t]*/, "", val); sub(/[ \t]+#[^"]*$/, "", val); sub(/[ \t]+$/, "", val)
  if (key in seenkey) fail_allow("key " key " is given twice in one item")
  seenkey[key] = 1
  if (key == "name") { iname = strval(val); if (iname == "\001" || iname == "") { fail_allow("name is not a string"); iname = "" } }
  else if (key == "reason") { ireason = strval(val); if (ireason == "\001") { fail_allow("reason is not a string"); ireason = "" } }
  else if (key == "task") { if (val ~ /^[0-9]+$/) itask = val; else fail_allow("task is not a number") }
  else fail_allow("unknown key " key)
  next
}
END {
  close_item()
  if (bad) exit 2
  for (i = 1; i <= na; i++) print anames[i]
}' "$@"
}

self_test() {
  fixtures=$here/check-it-budget-fixtures
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  fail=0
  # case, want exit, a string the output must have, then the arguments
  case_() {
    c=$1 want=$2 needle=$3
    shift 3
    got=0
    sh "$script" --allow "$fixtures/allow.toml" "$@" >"$tmp/out" 2>&1 || got=$?
    if [ "$got" -ne "$want" ]; then
      echo "$me --self-test: $c: exit $got, want $want" >&2
      sed 's/^/  /' "$tmp/out" >&2
      fail=1
    elif ! grep -qF -- "$needle" "$tmp/out"; then
      echo "$me --self-test: $c: no \"$needle\" in the output" >&2
      sed 's/^/  /' "$tmp/out" >&2
      fail=1
    else
      echo "$me --self-test: $c: exit $got: ok"
    fi
  }
  b=$fixtures/budget.toml
  case_ within_budget 0 "| tests | 4 | 5 | 0 |" --budget "$b" --report "$tmp/report.md" "$fixtures/within.xml"
  if ! grep -qF '| `slow_mod` | 1 | 2.000 |' "$tmp/report.md"; then
    echo "$me --self-test: within_budget: the report has no slow_mod row of the top modules" >&2; fail=1
  fi
  case_ within_budget_skips_other_binaries 0 "| seconds | 4.500 | 6.000 | 0.000 |" --budget "$b" "$fixtures/within.xml"
  case_ count_over 1 "| tests | 6 | 5 | 1 |" --budget "$b" "$fixtures/over-count.xml"
  case_ secs_over 1 "| seconds | 7.250 | 6.000 | 1.250 |" --budget "$b" "$fixtures/over-secs.xml"
  case_ secs_equal_to_the_budget 0 "Within the budget." --budget "$fixtures/budget-equal.toml" "$fixtures/within.xml"
  case_ report_not_writable 2 "cannot write the report" --budget "$b" --report "$tmp/no-such-dir/report.md" "$fixtures/within.xml"
  case_ junit_missing 2 "cannot read" --budget "$b" "$tmp/no-such-junit.xml"
  case_ junit_without_it 2 "no dagq::it testsuite" --budget "$b" "$fixtures/no-it-suite.xml"
  case_ junit_cut_off 2 "cut off" --budget "$b" "$fixtures/cut-off.xml"
  case_ budget_only 0 "budget ok" --budget "$b"
  case_ budget_malformed 1 "unknown key" --budget "$fixtures/budget-malformed.toml" "$fixtures/within.xml"
  case_ budget_missing 2 "no budget file" --budget "$tmp/no-such-budget.toml"
  case_ test_over 1 '| `dagq::it slow_mod::inner::unlisted` | 2.001 | 2.000 |' --budget "$b" --report "$tmp/over.md" "$fixtures/over-test.xml"
  if ! grep -qF '| `dagq::it other_mod::allowed` | 2.100 | 2.000 |' "$tmp/over.md"; then
    echo "$me --self-test: full_name_matching: report misses another module with the same fn name" >&2; fail=1
  fi
  case_ allowed_test_over 0 "Within the budget." --budget "$b" "$fixtures/allowed-test.xml"
  case_ test_within 0 "Within the budget." --budget "$b" "$fixtures/within-test.xml"
  case_ missing_test_secs 1 "no test_secs" --budget "$fixtures/budget-no-test-secs.toml"
  case_ bad_test_secs 1 "test_secs is not a number" --budget "$fixtures/budget-bad-test-secs.toml"
  case_ bad_allow 2 "has no reason" --budget "$b" --allow "$fixtures/allow-malformed.toml" "$fixtures/allowed-test.xml"
  case_ missing_allow 2 "cannot read the allow list" --budget "$b" --allow "$tmp/missing.toml" "$fixtures/allowed-test.xml"
  # The default union must be read from the cwd repository, even when this
  # script is copied out of it by a review program.
  repo=$tmp/repo
  mkdir -p "$repo/.config/it-slow-allow.d"
  git -C "$repo" init -q
  cp "$fixtures/allow.toml" "$repo/.config/it-slow-allow.toml"
  cp "$fixtures/allow-task.toml" "$repo/.config/it-slow-allow.d/t2.toml"
  got=0
  (cd "$repo" && sh "$script" --budget "$b" "$fixtures/allowed-union.xml") >"$tmp/out" 2>&1 || got=$?
  if [ "$got" -ne 0 ] || ! grep -qF "Within the budget." "$tmp/out"; then
    echo "$me --self-test: default_allow_union: failed (exit $got)" >&2; cat "$tmp/out" >&2; fail=1
  else
    echo "$me --self-test: default_allow_union: ok"
  fi
  cat "$fixtures/allow.toml" "$fixtures/allow-task.toml" >"$tmp/union.toml"
  case_ allowed_still_counts_in_totals 1 '| seconds | 5.000 | 4.500 | 0.500 |' --budget "$fixtures/budget-equal.toml" --allow "$tmp/union.toml" "$fixtures/allowed-union.xml"
  case_ allow_override_is_one_file 1 'dagq::it slow_mod::allowed_in_task_file' --budget "$b" "$fixtures/allowed-union.xml"
  cat "$fixtures/allow.toml" "$fixtures/allow.toml" >"$tmp/duplicate.toml"
  case_ duplicate_allow 2 "listed twice" --budget "$b" --allow "$tmp/duplicate.toml" "$fixtures/allowed-test.xml"
  if [ "$fail" -ne 0 ]; then exit 1; fi
  echo "$me --self-test: ok"
}

budget="" report="" junit="" allow=""
root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
while [ $# -gt 0 ]; do
  case $1 in
    --self-test) [ $# -eq 1 ] || usage; self_test; exit 0 ;;
    --budget) [ $# -ge 2 ] || usage; budget=$2; shift 2 ;;
    --allow) [ $# -ge 2 ] || usage; allow=$2; shift 2 ;;
    --report) [ $# -ge 2 ] || usage; report=$2; shift 2 ;;
    -*) usage ;;
    *) [ -z "$junit" ] || usage; junit=$1; shift ;;
  esac
done

if [ -z "$budget" ]; then
  budget=$root/.config/it-budget.toml
  shown=.config/it-budget.toml
fi
shown=${shown:-$budget}
if [ ! -f "$budget" ]; then
  echo "$me: no budget file $budget" >&2
  exit 2
fi
b=$(read_budget "$budget") || exit 1
if [ -z "$junit" ]; then
  echo "$me: budget ok ($budget)"
  exit 0
fi
if [ ! -r "$junit" ] || [ ! -f "$junit" ]; then
  echo "$me: cannot read the JUnit $junit" >&2
  exit 2
fi
tests=$(read_junit "$junit") || exit 2
if [ -n "$allow" ]; then
  set -- "$allow"
else
  set -- "$root/.config/it-slow-allow.toml"
  for f in "$root"/.config/it-slow-allow.d/*.toml; do
    [ -e "$f" ] || continue
    set -- "$@" "$f"
  done
fi
allowed=$(read_allow "$@") || exit 2

# The values go through the environment: -v would read the backslashes of
# the basis as escapes.
out=$(printf '%s\n' "$tests" | IT_ALLOW="$allowed" IT_BUDGET="$b" IT_BUDGET_FILE="$shown" awk -F'\t' '
  BEGIN {
    split(ENVIRON["IT_BUDGET"], v, "\t"); bc = v[1] + 0; bs = v[2] + 0; bt = v[3] + 0; decided = v[4]; basis = v[5]
    na = split(ENVIRON["IT_ALLOW"], names, "\n")
    for (i = 1; i <= na; i++) allowed[names[i]] = 1
    file = ENVIRON["IT_BUDGET_FILE"]
  }
  {
    n++; s += $2
    if (int($2 * 1000 + 0.5) > int(bt * 1000 + 0.5) && !($1 in allowed)) {
      slow[++ns] = $1; seconds[ns] = $2
    }
    m = $1; sub(/::.*/, "", m)
    mn[m]++; ms[m] += $2
  }
  END {
    # The seconds are compared in whole milliseconds, as JUnit gives them, so
    # a float sum equal to the budget is not over it.
    oc = n - bc; os = (int(s * 1000 + 0.5) - int(bs * 1000 + 0.5)) / 1000
    print "## dagq::it budget"
    print ""
    print "| | measured | budget | over |"
    print "| --- | ---: | ---: | ---: |"
    printf "| tests | %d | %d | %d |\n", n, bc, (oc > 0 ? oc : 0)
    printf "| seconds | %.3f | %.3f | %.3f |\n", s, bs, (os > 0 ? os : 0)
    print ""
    printf "The budget: %s, decided %s (%s).\n", file, decided, basis
    print ""
    if (oc > 0 || os > 0 || ns > 0) print "**Over the budget.**"; else print "Within the budget."
    print ""
    printf "Per-test ceiling: %.3fs (allow-listed tests exempt).\n", bt
    if (ns > 0) {
      print ""
      print "Tests over the per-test ceiling without an allow-list item:"
      print ""
      print "| test | seconds | ceiling |"
      print "| --- | ---: | ---: |"
      for (i = 1; i <= ns; i++) printf "| `dagq::it %s` | %.3f | %.3f |\n", slow[i], seconds[i], bt
    }
    print ""
    print "The 10 modules with the most seconds:"
    print ""
    print "| module | tests | seconds |"
    print "| --- | ---: | ---: |"
    for (k = 1; k <= 10; k++) {
      best = ""
      for (m in ms) if (!(m in done) && (best == "" || ms[m] > ms[best] || (ms[m] == ms[best] && m < best))) best = m
      if (best == "") break
      done[best] = 1
      printf "| `%s` | %d | %.3f |\n", best, mn[best], ms[best]
    }
    if (oc > 0 || os > 0 || ns > 0) exit 1
  }') || st=$?
st=${st:-0}
printf '%s\n' "$out"
if [ -n "$report" ]; then
  printf '%s\n' "$out" >"$report" || { echo "$me: cannot write the report $report" >&2; exit 2; }
fi
exit "$st"
