#!/bin/sh
# Compare selected nextest JUnit suites with a nightly budget. Defaults to
# dagq::it and .config/it-budget.toml; --suite dagq --suite dagq::bin/dagq
# selects the lib/bin unit suites for .config/lib-budget.toml. Nightly only:
# this is not a landing or ci.yml gate. Rules: testing.md, section "CI".
#
# Usage: sh scripts/check-it-budget.sh [--budget FILE] [--suite NAME ...]
#        [--allow FILE] [--report FILE] [JUNIT] | --self-test
# --budget defaults to the cwd repository's .config/it-budget.toml (or the
# script's repository outside git). --suite is repeatable, exact suite names;
# absent, selects dagq::it. --report also writes the Markdown report to FILE.
# Without JUNIT, validates only the budget. --self-test runs the fixtures.
# --allow reads one allow file instead of the cwd repository's union of
# .config/it-ceiling-allow.toml and .config/it-ceiling-allow.d/*.toml. Allow lists
# are read only with test_secs; their format matches check-it-test-time.sh.
# This ceiling list is independent of the five-second it-slow-allow lists.
# New items go in .config/it-ceiling-allow.d/t<task ID>.toml, with the full
# name, reason (ADR-t1707-1 decision 2) and registering task.
#
# Budget: one key = value per line, with # comments. Required, once each:
# count = nonnegative integer, secs = nonnegative number, decided = quoted
# YYYY-MM-DD, basis = nonempty quoted string. Optional test_secs is a
# nonnegative per-test ceiling (allow-listed full test names are exempt).
# Optional top_n (positive integer) and top_secs (nonnegative number) must
# appear together: compare the sum of the slowest N testcases. No other keys.
# Omitting these optional ceilings preserves the four-key budget format.
#
# Each testcase in the selected suites counts once, with its time attribute
# (the attempt nextest reports; ignored/unbuilt tests are absent). All tests,
# including allowed ones, count in totals and top N. Report: totals, ceilings,
# excess, top 10 modules, unlisted tests over test_secs, and the top N full
# names (suite + test) and seconds when top_n is present. Module is the first
# segment of the test name. Duplicate names in different suites count apart.
#
# Exit 0: within budget, or budget-only validation passed. Exit 1: malformed
# budget or any ceiling exceeded. Equality passes, seconds rounded to whole
# milliseconds. Exit 2: usage, missing budget, unreadable/malformed allow
# list, unwritable report, missing/unreadable/cut-off JUnit (no </testsuites>)
# or no testcase in any selected suite. Requires sh, POSIX awk and git.
set -eu

me=check-it-budget
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "usage: sh scripts/check-it-budget.sh [--budget FILE] [--suite NAME ...] [--allow FILE] [--report FILE] [JUNIT] | --self-test" >&2
  exit 2
}

# Prints "count secs test_secs decided basis top_n top_secs" checked, or the problems on stderr and
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
      } else if (key == "top_n") {
        if (val !~ /^[0-9]+$/ || val + 0 == 0) bad("top_n is not a positive integer: " val); else top_n = val
      } else if (key == "top_secs") {
        if (val !~ /^[0-9]+(\.[0-9]+)?$/) bad("top_secs is not a number: " val); else top_secs = val
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
      if (("top_n" in seen) != ("top_secs" in seen)) { bad("top_n and top_secs must appear together") }
      if (err) exit 1
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n", count, secs, test_secs, decided, basis, top_n, top_secs
    }' "$1"
}

# Prints "test<TAB>secs<TAB>suite" per selected testcase, or exits 2 with the reason.
read_junit() {
  awk -v me="$me" -v file="$1" -v suites="$suites" '
    BEGIN { nsel = split(suites, selected, " "); for (i = 1; i <= nsel; i++) wanted[selected[i]] = 1 }
    /<testsuite[ >]/ {
      suite = ""
      if (match($0, / name="[^"]*"/)) suite = substr($0, RSTART + 7, RLENGTH - 8)
      insuite = (suite in wanted)
      if (insuite) found = 1
      if ($0 ~ /<testsuite [^>]*\/>/) insuite = 0
    }
    insuite && /<testcase[ >]/ {
      if (match($0, / name="[^"]*"/) == 0) { print me ": " file ":" NR ": a testcase without a name" > "/dev/stderr"; bad = 1; next }
      name = substr($0, RSTART + 7, RLENGTH - 8)
      if (match($0, / time="[0-9.]+"/) == 0) { print me ": " file ":" NR ": " name " has no time" > "/dev/stderr"; bad = 1; next }
      printf "%s\t%s\t%s\n", name, substr($0, RSTART + 7, RLENGTH - 8), suite
      n++
    }
    /<\/testsuite>/ { insuite = 0 }
    /<\/testsuites>/ { closed = 1 }
    END {
      if (bad) exit 2
      if (!closed) { print me ": " file ": no </testsuites> (cut off)" > "/dev/stderr"; exit 2 }
      if (!found || n == 0) { print me ": " file ": no " suites " testsuite with a test in it" > "/dev/stderr"; exit 2 }
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
  case_ four_key_budget 0 "budget ok" --budget "$fixtures/budget-no-test-secs.toml"
  case_ bad_test_secs 1 "test_secs is not a number" --budget "$fixtures/budget-bad-test-secs.toml"
  case_ bad_allow 2 "has no reason" --budget "$b" --allow "$fixtures/allow-malformed.toml" "$fixtures/allowed-test.xml"
  case_ missing_allow 2 "cannot read the allow list" --budget "$b" --allow "$tmp/missing.toml" "$fixtures/allowed-test.xml"
  case_ four_key_budget_with_junit 0 "Within the budget." --budget "$fixtures/budget-no-test-secs.toml" "$fixtures/over-test.xml"
  case_ lib_equal 0 '| top 20 seconds | 250.000 | 250.000 | 0.000 |' --suite dagq --suite dagq::bin/dagq --budget "$fixtures/lib.toml" --report "$tmp/lib.md" "$fixtures/lib.xml"
  # The top 20 must be descending, include the bin suite, and omit the two
  # fastest tests. Expected values are fixed fixture arithmetic, not a
  # second invocation of the implementation to compute a baseline.
  if ! awk '
    /^The 20 tests/ { top = 1; next }
    top && /^The 10 modules/ { top = 0 }
    top && /^\| `/ {
      n++
      if (n == 1 && $0 != "| `dagq::bin/dagq unit::t22` | 22.000 |") bad = 1
      if (n == 20 && $0 != "| `dagq unit::t3` | 3.000 |") bad = 1
      if ($0 ~ /unit::t[12]`/) bad = 1
    }
    END { exit (bad || n != 20) }
  ' "$tmp/lib.md"; then
    echo "$me --self-test: lib_top_report: wrong top 20" >&2; fail=1
  fi
  case_ lib_count_over 1 '| tests | 22 | 21 | 1 |' --suite dagq --suite dagq::bin/dagq --budget "$fixtures/lib-over-count.toml" "$fixtures/lib.xml"
  case_ lib_secs_over 1 '| seconds | 253.000 | 252.000 | 1.000 |' --suite dagq --suite dagq::bin/dagq --budget "$fixtures/lib-over-secs.toml" "$fixtures/lib.xml"
  case_ lib_top_over 1 '| top 20 seconds | 250.000 | 249.000 | 1.000 |' --suite dagq --suite dagq::bin/dagq --budget "$fixtures/lib-over-top.toml" "$fixtures/lib.xml"
  case_ lib_within 0 'Within the budget.' --suite dagq --budget "$fixtures/lib.toml" "$fixtures/lib.xml"
  case_ selected_suite_missing 2 'no unknown testsuite' --suite unknown --budget "$fixtures/lib.toml" "$fixtures/lib.xml"
  case_ missing_top_secs 1 'must appear together' --budget "$fixtures/missing-top-secs.toml"
  case_ missing_top_n 1 'must appear together' --budget "$fixtures/missing-top-n.toml"
  case_ bad_top_n 1 'not a positive integer' --budget "$fixtures/bad-top-n.toml"
  case_ bad_top_secs 1 'not a number' --budget "$fixtures/bad-top-secs.toml"
  # The default union must be read from the cwd repository, even when this
  # script is copied out of it by a review program.
  repo=$tmp/repo
  mkdir -p "$repo/.config/it-ceiling-allow.d"
  git -C "$repo" init -q
  cp "$fixtures/allow.toml" "$repo/.config/it-ceiling-allow.toml"
  cp "$fixtures/allow-task.toml" "$repo/.config/it-ceiling-allow.d/t2.toml"
  got=0
  (cd "$repo" && sh "$script" --budget "$b" "$fixtures/allowed-union.xml") >"$tmp/out" 2>&1 || got=$?
  if [ "$got" -ne 0 ] || ! grep -qF "Within the budget." "$tmp/out"; then
    echo "$me --self-test: default_allow_union: failed (exit $got)" >&2; cat "$tmp/out" >&2; fail=1
  else
    echo "$me --self-test: default_allow_union: ok"
  fi
  # Exercise default discovery with independent, fixed 30-second fixtures.
  # A five-second exemption alone must never grant a nightly exemption.
  mkdir -p "$repo/.config/it-slow-allow.d"
  cp "$fixtures/slow-only-allow.toml" "$repo/.config/it-slow-allow.toml"
  cp "$fixtures/allow.toml" "$repo/.config/it-slow-allow.d/t3.toml"
  cp "$fixtures/ceiling-allow.toml" "$repo/.config/it-ceiling-allow.d/t3.toml"
  default_case() {
    c=$1 want=$2 needle=$3 junit=$4
    got=0
    (cd "$repo" && sh "$script" --budget "$fixtures/budget-ceiling.toml" "$junit") >"$tmp/out" 2>&1 || got=$?
    if [ "$got" -ne "$want" ] || ! grep -qF "$needle" "$tmp/out"; then
      echo "$me --self-test: $c: failed (exit $got, want $want)" >&2
      cat "$tmp/out" >&2; fail=1
    else
      echo "$me --self-test: $c: exit $got: ok"
    fi
  }
  default_case slow_gate_only_over_ceiling 1 '| `dagq::it ceiling::slow_only` | 30.001 | 30.000 |' "$fixtures/slow-only.xml"
  default_case ceiling_allowed_over 0 'Within the budget.' "$fixtures/ceiling-allowed.xml"
  default_case ceiling_equal 0 'Within the budget.' "$fixtures/ceiling-equal.xml"
  cat "$fixtures/allow.toml" "$fixtures/allow-task.toml" >"$tmp/union.toml"
  case_ allowed_still_counts_in_totals 1 '| seconds | 5.000 | 4.500 | 0.500 |' --budget "$fixtures/budget-equal.toml" --allow "$tmp/union.toml" "$fixtures/allowed-union.xml"
  case_ allow_override_is_one_file 1 'dagq::it slow_mod::allowed_in_task_file' --budget "$b" "$fixtures/allowed-union.xml"
  cat "$fixtures/allow.toml" "$fixtures/allow.toml" >"$tmp/duplicate.toml"
  case_ duplicate_allow 2 "listed twice" --budget "$b" --allow "$tmp/duplicate.toml" "$fixtures/allowed-test.xml"
  if [ "$fail" -ne 0 ]; then exit 1; fi
  echo "$me --self-test: ok"
}

budget="" report="" junit="" allow="" suites=""
root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
while [ $# -gt 0 ]; do
  case $1 in
    --self-test) [ $# -eq 1 ] || usage; self_test; exit 0 ;;
    --budget) [ $# -ge 2 ] || usage; budget=$2; shift 2 ;;
    --suite) [ $# -ge 2 ] && [ -n "$2" ] || usage; suites="${suites:+$suites }$2"; shift 2 ;;
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
suites=${suites:-dagq::it}
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
# A lib budget has no per-test ceiling and needs no IT allow list.
has_test_secs=$(printf '%s\n' "$b" | awk -F'\t' '{ print ($3 != "") }')
allowed=""
if [ "$has_test_secs" = 1 ]; then
  if [ -n "$allow" ]; then
    set -- "$allow"
  else
    set -- "$root/.config/it-ceiling-allow.toml"
    for f in "$root"/.config/it-ceiling-allow.d/*.toml; do
      [ -e "$f" ] || continue
      set -- "$@" "$f"
    done
  fi
  allowed=$(read_allow "$@") || exit 2
fi

# The values go through the environment: -v would read the backslashes of
# the basis as escapes.
out=$(printf '%s\n' "$tests" | IT_SUITES="$suites" IT_ALLOW="$allowed" IT_BUDGET="$b" IT_BUDGET_FILE="$shown" awk -F'\t' '
  BEGIN {
    split(ENVIRON["IT_BUDGET"], v, "\t"); bc = v[1] + 0; bs = v[2] + 0; bt = v[3] + 0; decided = v[4]; basis = v[5]; tn = v[6] + 0; ts = v[7] + 0; per_test = (v[3] != "")
    na = split(ENVIRON["IT_ALLOW"], names, "\n")
    for (i = 1; i <= na; i++) allowed[names[i]] = 1
    file = ENVIRON["IT_BUDGET_FILE"]
  }
  {
    n++; s += $2
    full[n] = $3 " " $1; times[n] = $2
    if (per_test && int($2 * 1000 + 0.5) > int(bt * 1000 + 0.5) && !($1 in allowed)) {
      slow[++ns] = $3 " " $1; seconds[ns] = $2
    }
    m = $1; sub(/::.*/, "", m)
    mn[m]++; ms[m] += $2
  }
  END {
    # The seconds are compared in whole milliseconds, as JUnit gives them, so
    # a float sum equal to the budget is not over it.
    oc = n - bc; os = (int(s * 1000 + 0.5) - int(bs * 1000 + 0.5)) / 1000
    top = 0
    for (k = 1; k <= tn && k <= n; k++) {
      best = 0
      for (i = 1; i <= n; i++) if (!(i in picked) && (!best || times[i] > times[best] || (times[i] == times[best] && full[i] < full[best]))) best = i
      picked[best] = 1; rank[k] = best; top += times[best]
    }
    ot = (int(top * 1000 + 0.5) - int(ts * 1000 + 0.5)) / 1000
    over = (oc > 0 || os > 0 || ns > 0 || (tn > 0 && ot > 0))
    print "## " ENVIRON["IT_SUITES"] " budget"
    print ""
    print "| | measured | budget | over |"
    print "| --- | ---: | ---: | ---: |"
    printf "| tests | %d | %d | %d |\n", n, bc, (oc > 0 ? oc : 0)
    printf "| seconds | %.3f | %.3f | %.3f |\n", s, bs, (os > 0 ? os : 0)
    if (tn > 0) printf "| top %d seconds | %.3f | %.3f | %.3f |\n", tn, top, ts, (ot > 0 ? ot : 0)
    print ""
    printf "The budget: %s, decided %s (%s).\n", file, decided, basis
    print ""
    if (over) print "**Over the budget.**"; else print "Within the budget."
    print ""
    if (per_test) printf "Per-test ceiling: %.3fs (allow-listed tests exempt).\n", bt
    if (ns > 0) {
      print ""
      print "Tests over the per-test ceiling without an allow-list item:"
      print ""
      print "| test | seconds | ceiling |"
      print "| --- | ---: | ---: |"
      for (i = 1; i <= ns; i++) printf "| `%s` | %.3f | %.3f |\n", slow[i], seconds[i], bt
    }
    print ""
    if (tn > 0) {
      printf "The %d tests with the most seconds:\n\n", tn
      print "| test | seconds |"
      print "| --- | ---: |"
      for (k = 1; k <= tn && k <= n; k++) printf "| `%s` | %.3f |\n", full[rank[k]], times[rank[k]]
      print ""
    }
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
    if (over) exit 1
  }') || st=$?
st=${st:-0}
printf '%s\n' "$out"
if [ -n "$report" ]; then
  printf '%s\n' "$out" >"$report" || { echo "$me: cannot write the report $report" >&2; exit 2; }
fi
exit "$st"
