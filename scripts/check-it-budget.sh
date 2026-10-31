#!/bin/sh
# The budget of the integration tests (dagq::it, the binary of tests/it):
# the number of its tests and the sum of their seconds that one cargo nextest
# JUnit file records, against the ceiling in .config/it-budget.toml. The
# nightly workflow .github/workflows/it-budget.yml runs it on main; the rule
# and how an over-budget night becomes a fix is in docs/development/
# testing.md, section "CI". It is not a gate of the landing nor of ci.yml.
#
# Usage: sh scripts/check-it-budget.sh [--budget FILE] [--report FILE] [JUNIT]
#        sh scripts/check-it-budget.sh --self-test
#   --budget FILE  the budget (default: .config/it-budget.toml of the git
#                  work tree of the cwd, or of the script's repository when
#                  the cwd is in none)
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
#   decided = "<YYYY-MM-DD, the day the values were set>"
#   basis   = "<what the values were set from>"
# All four keys, each once, and no other key. A file that breaks this is a
# malformed budget (exit 1).
#
# The JUnit: every <testcase> inside the <testsuite name="dagq::it"> counts
# once, with the seconds of its time attribute (nextest writes one testcase
# per test, the time of the attempt it reports; #[ignore]d tests and tests
# not built for the platform are not in the file). The module of a test is
# the first segment of its name, the file or directory under tests/it. The
# report names the count, the seconds, the budget, what is over it and the
# 10 modules with the most seconds.
#
# Exit status: 0 when the run is within the budget (or, without JUNIT, the
# budget file is well-formed), 1 when the count or the seconds is over the
# budget or the budget file is malformed, 2 on a usage error, a missing
# budget file, a report that cannot be written, or a JUnit that is missing,
# unreadable, cut off (no </testsuites>) or without a dagq::it testsuite with
# a test in it. A value equal to the budget is not over it (the seconds in
# whole milliseconds). Only sh and a POSIX awk are needed.
set -eu

me=check-it-budget
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "usage: sh scripts/check-it-budget.sh [--budget FILE] [--report FILE] [JUNIT] | --self-test" >&2
  exit 2
}

# Prints "count secs decided basis" checked, or the problems on stderr and
# exits 1.
read_budget() {
  awk -v me="$me" -v file="$1" '
    function bad(msg) { print me ": " file ":" NR ": " msg > "/dev/stderr"; err = 1 }
    /^[ \t]*(#|$)/ { next }
    {
      line = $0
      sub(/[ \t]+#[^"]*$/, "", line)
      if (match(line, /^[ \t]*[a-z]+[ \t]*=[ \t]*/) == 0) { bad("not a key = value line"); next }
      key = substr(line, RSTART, RLENGTH); sub(/[ \t]*=.*/, "", key); sub(/^[ \t]*/, "", key)
      val = substr(line, RLENGTH + 1); sub(/[ \t]+$/, "", val)
      if (key in seen) { bad("key " key " twice"); next }
      seen[key] = 1
      if (key == "count") {
        if (val !~ /^[0-9]+$/) bad("count is not an integer: " val); else count = val
      } else if (key == "secs") {
        if (val !~ /^[0-9]+(\.[0-9]+)?$/) bad("secs is not a number: " val); else secs = val
      } else if (key == "decided") {
        if (val !~ /^"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]"$/) bad("decided is not a quoted YYYY-MM-DD: " val)
        else decided = substr(val, 2, 10)
      } else if (key == "basis") {
        if (val !~ /^".+"$/) bad("basis is not a quoted string: " val); else basis = substr(val, 2, length(val) - 2)
      } else bad("unknown key " key)
    }
    END {
      n = split("count secs decided basis", keys, " ")
      for (i = 1; i <= n; i++) if (!(keys[i] in seen)) { print me ": " file ": no " keys[i] > "/dev/stderr"; err = 1 }
      if (err) exit 1
      printf "%s\t%s\t%s\t%s\n", count, secs, decided, basis
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
    sh "$script" "$@" >"$tmp/out" 2>&1 || got=$?
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
  if [ "$fail" -ne 0 ]; then exit 1; fi
  echo "$me --self-test: ok"
}

budget="" report="" junit=""
while [ $# -gt 0 ]; do
  case $1 in
    --self-test) [ $# -eq 1 ] || usage; self_test; exit 0 ;;
    --budget) [ $# -ge 2 ] || usage; budget=$2; shift 2 ;;
    --report) [ $# -ge 2 ] || usage; report=$2; shift 2 ;;
    -*) usage ;;
    *) [ -z "$junit" ] || usage; junit=$1; shift ;;
  esac
done

if [ -z "$budget" ]; then
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
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

# The values go through the environment: -v would read the backslashes of
# the basis as escapes.
out=$(printf '%s\n' "$tests" | IT_BUDGET="$b" IT_BUDGET_FILE="$shown" awk -F'\t' '
  BEGIN {
    split(ENVIRON["IT_BUDGET"], v, "\t"); bc = v[1] + 0; bs = v[2] + 0; decided = v[3]; basis = v[4]
    file = ENVIRON["IT_BUDGET_FILE"]
  }
  {
    n++; s += $2
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
    if (oc > 0 || os > 0) print "**Over the budget.**"; else print "Within the budget."
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
    if (oc > 0 || os > 0) exit 1
  }') || st=$?
st=${st:-0}
printf '%s\n' "$out"
if [ -n "$report" ]; then
  printf '%s\n' "$out" >"$report" || { echo "$me: cannot write the report $report" >&2; exit 2; }
fi
exit "$st"
