#!/bin/sh
# Summarise the slow tests of cargo nextest output (goal 68): the slowest
# tests, how many took more than 1, 5 and 30 seconds and how long they took
# together, the count and total of every timed test, and the count and total
# of each test binary (goal 118). The output is Markdown tables, printed to
# stdout; CI appends it to $GITHUB_STEP_SUMMARY.
#
# Usage: sh scripts/slow-tests.sh [--top N] [--min-ratio R] [LOG...]
#        sh scripts/slow-tests.sh --self-test
#   LOG            nextest output (a file, or - for stdin); stdin when none.
#                  The dagq integrate logs <runs_dir>/*/integrate-*-verify-*.log
#                  are read as they are
#   --top N        how many of the slowest tests to list (default 20)
#   --min-ratio R  leave out a log with fewer timed tests than R times the
#                  tests it started ("Starting N tests"), or, when it has no
#                  such line, than R times the most timed tests of any log:
#                  a run stopped midway (default 0.9)
#   --self-test    check the tables against the fixtures under
#                  scripts/slow-tests-fixtures/ (exit 0 when they match)
#
# A test's time is read from its PASS line and, for a test that passed on a
# retry, from its FLKY-FL (or FLAKY) line; FAIL, TRY, SLOW and SKIP lines are
# not timed. With several logs the time of a test is its median over the logs
# counted. Colour codes are removed, so --color always output reads the same.
# A test's binary is the first word of its name, nextest's binary id (dagq::it,
# dagq for the lib, a crate's name); a binary's total is the sum of the times
# (medians) of its tests.
# Only sh and a POSIX awk are needed (macOS and ubuntu).
#
# Exit status: 0 with the summary (also when no test was timed), 2 on a usage
# error or an unreadable log.
set -eu

top=20
ratio=0.9
given=0

# self_test runs the script on the fixtures and compares its whole output
# with the expected Markdown, then checks the exit status of usage errors.
self_test() {
  dir=$(dirname "$0")/slow-tests-fixtures
  fail=0
  out=$(mktemp "${TMPDIR:-/tmp}/slow-tests-self-test.XXXXXX")
  check() { # name expected-file args...
    name=$1; want=$2; shift 2
    if sh "$0" "$@" > "$out" && diff -u "$want" "$out" >&2; then
      echo "slow-tests --self-test: $name: ok"
    else
      echo "slow-tests --self-test: $name: FAILED" >&2; fail=1
    fi
  }
  check one_log "$dir/one.md" "$dir/one.log"
  check two_logs_median "$dir/two.md" --top 3 "$dir/one.log" "$dir/two.log"
  check stdin "$dir/one.md" - < "$dir/one.log"
  check stopped_log_left_out "$dir/stopped.md" --min-ratio 0.9 "$dir/one.log" "$dir/stopped.log"
  for bad in "--top x" "--min-ratio 1.2.3" "--unknown" "$dir/missing.log" "--top 3 --self-test"; do
    # shellcheck disable=SC2086 # the words of a case are its arguments
    if sh "$0" $bad > /dev/null 2>&1; then status=0; else status=$?; fi
    if [ "$status" -eq 2 ]; then echo "slow-tests --self-test: $bad: exit 2: ok"
    else echo "slow-tests --self-test: $bad: exit $status, want 2" >&2; fail=1; fi
  done
  rm -f "$out"
  if [ "$fail" -eq 0 ]; then echo "slow-tests --self-test: ok"; else exit 1; fi
}

while [ $# -gt 0 ]; do
  case "$1" in
    --top) top=${2:?--top needs a number}; given=1; shift 2 ;;
    --min-ratio) ratio=${2:?--min-ratio needs a number}; given=1; shift 2 ;;
    --self-test)
      [ $# -eq 1 ] && [ "$given" -eq 0 ] || { echo "slow-tests: --self-test takes no other argument" >&2; exit 2; }
      self_test; exit 0 ;;
    -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
    --) shift; break ;;
    -?*) echo "slow-tests: unknown argument: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done

case "$top" in
  ''|*[!0-9]*) echo "slow-tests: --top is not a number: $top" >&2; exit 2 ;;
esac
case "$ratio" in
  ''|*[!0-9.]*|*.*.*) echo "slow-tests: --min-ratio is not a number: $ratio" >&2; exit 2 ;;
esac

for log in "$@"; do
  if [ "$log" != - ] && [ ! -r "$log" ]; then
    echo "slow-tests: cannot read $log" >&2
    exit 2
  fi
done

awk -v top="$top" -v ratio="$ratio" '
function secs_of(line,    i, j) {
  i = index(line, "[")
  j = index(line, "s]")
  if (i == 0 || j <= i) return ""
  return substr(line, i + 1, j - i - 1) + 0
}
function name_of(line,    rest, k) {
  rest = substr(line, index(line, "s]") + 2)
  sub(/^ +/, "", rest)
  if (substr(rest, 1, 1) == "(") {
    k = index(rest, ")")
    rest = substr(rest, k + 1)
    sub(/^ +/, "", rest)
  }
  sub(/ +$/, "", rest)
  return rest
}
function pct(part, whole) {
  return whole > 0 ? sprintf("%.0f%%", 100 * part / whole) : "-"
}
FNR == 1 { nlogs++ }
{
  line = $0
  gsub(/\033\[[0-9;]*[A-Za-z]/, "", line)
  gsub(/\r/, "", line)
  split(line, f, " ")
  if (f[1] == "Starting" && f[2] ~ /^[0-9]+$/ && f[3] == "tests") {
    started[nlogs] = f[2] + 0
    next
  }
  if (f[1] != "PASS" && f[1] != "FLKY-FL" && f[1] != "FLAKY") next
  if (index(line, "[") == 0 || index(line, "[>") > 0) next
  s = secs_of(line)
  if (s == "") next
  name = name_of(line)
  if (name == "") next
  key = nlogs SUBSEP name
  if (!(key in t)) {
    timed[nlogs]++
    if (!(name in seen)) { seen[name] = 1; names[++nnames] = name }
  }
  t[key] = s
}
END {
  most = 0
  for (l = 1; l <= nlogs; l++) if (timed[l] > most) most = timed[l]
  used = 0; withtimes = 0
  for (l = 1; l <= nlogs; l++) {
    if (timed[l] > 0) withtimes++
    want = (l in started) ? started[l] : most
    ok[l] = (timed[l] > 0 && timed[l] >= ratio * want)
    if (ok[l]) used++
  }

  n = 0; total = 0
  split("1 5 30", th, " ")
  for (k = 1; k <= 3; k++) { over[k] = 0; oversum[k] = 0 }
  for (i = 1; i <= nnames; i++) {
    name = names[i]
    m = 0
    for (l = 1; l <= nlogs; l++) {
      if (!ok[l] || !((l SUBSEP name) in t)) continue
      v = t[l, name]
      # insertion sort of the few values of one test
      for (j = m; j >= 1 && vals[j] > v; j--) vals[j + 1] = vals[j]
      vals[j + 1] = v
      m++
    }
    if (m == 0) continue
    med = (m % 2) ? vals[(m + 1) / 2] : (vals[m / 2] + vals[m / 2 + 1]) / 2
    n++
    tname[n] = name; tsec[n] = med; total += med
    for (k = 1; k <= 3; k++) if (med > th[k]) { over[k]++; oversum[k] += med }
  }

  print "## 遅い test（nextest）"
  print ""
  printf "test の時間の出た log %d 本のうち %d 本を数えた（途中で止まった log を除く）。", withtimes, used
  if (used > 1) printf "test ごとの秒は log をまたいだ中央値。"
  print ""
  print ""
  if (n == 0) {
    print "数えた test がない（test の時間の出た log が無いか、どれも途中で止まっている）。"
    exit 0
  }
  print "| 範囲 | 本数 | 合計（秒） | 全体の合計に占める割合 |"
  print "| --- | ---: | ---: | ---: |"
  printf "| 全体 | %d | %.1f | %s |\n", n, total, pct(total, total)
  for (k = 1; k <= 3; k++)
    printf "| %d 秒を超える | %d | %.1f | %s |\n", th[k], over[k], oversum[k], pct(oversum[k], total)
  print ""
  shown = (top < n) ? top : n
  printf "### 上位 %d 本\n\n", shown
  print "| # | 秒 | test |"
  print "| ---: | ---: | --- |"
  for (r = 1; r <= shown; r++) {
    best = 0
    for (i = 1; i <= n; i++)
      if (!(i in taken) && (best == 0 || tsec[i] > tsec[best])) best = i
    taken[best] = 1
    printf "| %d | %.3f | `%s` |\n", r, tsec[best], tname[best]
  }

  # per test binary: the first word of the name is the nextest binary id
  nb = 0
  for (i = 1; i <= n; i++) {
    b = tname[i]
    sub(/ .*/, "", b)
    if (!(b in bcount)) bnames[++nb] = b
    bcount[b]++; bsum[b] += tsec[i]
  }
  print ""
  print "### test binary ごと"
  print ""
  print "| test binary | 本数 | 合計（秒） | 全体の合計に占める割合 |"
  print "| --- | ---: | ---: | ---: |"
  for (r = 1; r <= nb; r++) {
    best = 0
    for (i = 1; i <= nb; i++)
      if (!(i in btaken) && (best == 0 || bsum[bnames[i]] > bsum[bnames[best]] ||
          (bsum[bnames[i]] == bsum[bnames[best]] && bnames[i] < bnames[best]))) best = i
    btaken[best] = 1
    b = bnames[best]
    printf "| `%s` | %d | %.1f | %s |\n", b, bcount[b], bsum[b], pct(bsum[b], total)
  }
}
' "$@"
