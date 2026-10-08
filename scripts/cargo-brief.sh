#!/bin/sh
# Run a cargo command for a worker's local checks, keep its whole output in a
# file under ${TMPDIR}, and print only what a worker reads: the failed tests
# with their output, the compiler's and clippy's diagnostics, the fmt diff,
# the summary lines and the log's path. The rule of which checks a worker
# runs is in docs/development/local-checks.md, section "workerの手元の検証";
# this script only shortens what they print.
#
# Usage: sh scripts/cargo-brief.sh [--log FILE] COMMAND...
#        sh scripts/cargo-brief.sh --self-test
#   COMMAND     the command as it would be typed, e.g. cargo test --locked
#               --lib queue, cargo nextest run --locked --test it
#               --stress-count 5 -E 'test(=m::t)', cargo clippy --locked
#               --all-targets -- -D warnings, cargo fmt --all --check
#   --log FILE  write the whole output (stdout and stderr in the order they
#               came) to FILE; by default a new file under ${TMPDIR:-/tmp}
#   --self-test check the script on the recorded outputs under
#               scripts/cargo-brief-fixtures/ and compare what it prints with
#               the expected *.out files there
#
# What it prints, in this order:
# - the failures and diagnostics: a cargo test failure's "---- name stdout
#   ----" block and the list after "failures:"; a nextest status line that is
#   not a pass (FAIL, SIGSEGV, TIMEOUT, ...) with the stdout and stderr under
#   it (in a stress run, a test's output only the first time it fails; each
#   retry's output is kept); each rustc or clippy error ("error..." up to the
#   blank line after it, a "Caused by:" block with a build script's output)
#   and a "thread '...' panicked" block outside the tests; for cargo fmt,
#   every line of the diff. Compile progress, passed tests and nextest's
#   per-iteration lines are left out. When the command failed and no failure
#   or error was found, the last 40 lines of the log. Then the warnings
#   ("warning..." up to the blank line), after the rest so that they cannot
#   push a failure out of the limit below.
# - at most CARGO_BRIEF_MAX_LINES lines (default 150) and
#   CARGO_BRIEF_MAX_BYTES bytes (default 12000) of them; the rest is left out
#   with a line that says how many lines and bytes and where the log is.
# - the summary: cargo test's "test result:" lines, nextest's last "Summary"
#   line with the lines after it, cargo fmt's count of diffs, and otherwise
#   cargo's last "Finished" line (the last 20 lines when there are more).
# - a last line with the exit status and the log's path, line count and
#   bytes.
#
# To read more of a failure, read the log narrowly (rg -n PATTERN LOG,
# tail -n N LOG) rather than whole. Only sh and a POSIX awk are needed.
#
# Exit status: the command's, or 2 on a usage error or a log that cannot be
# written. --self-test exits 0 when every case prints what is expected.
set -eu

me=cargo-brief
here=$(cd -P -- "$(dirname -- "$0")" >/dev/null && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/cargo-brief.sh [--log FILE] COMMAND... | --self-test" >&2
  exit 2
}

# kind_of prints how to read the command's output: fmt, or cargo for the
# rest (cargo test, nextest, clippy and builds share one reading).
kind_of() {
  for a in "$@"; do
    case "$a" in
      fmt) echo fmt; return ;;
      test|nextest|clippy|build|check) echo cargo; return ;;
    esac
  done
  echo cargo
}

# brief reads the log and prints the shortened output. $1 is the kind, $2
# the log, $3 the command's exit status.
brief() {
  BRIEF_KIND=$1 BRIEF_LOG=$2 BRIEF_STATUS=$3 \
  BRIEF_MAX_LINES=${CARGO_BRIEF_MAX_LINES:-150} \
  BRIEF_MAX_BYTES=${CARGO_BRIEF_MAX_BYTES:-12000} \
  LC_ALL=C awk '
# A detail goes to d, or to w when it belongs to a warning, so that warnings
# come after the errors and failures and cannot push them out of the limit.
# Consecutive blank lines are kept as one.
function detail(s) {
  if (warn) { if (s == "" && (nw == 0 || w[nw] == "")) return; nw++; w[nw] = s; return }
  if (s == "" && (nd == 0 || d[nd] == "")) return
  nd++; d[nd] = s
}
function summary(s) { ns++; sm[ns] = s }
# A nextest status line: "        FAIL [   0.012s] (1/2) crate test".
function nstatus(s) { return s ~ /^ +[A-Z][A-Z0-9 \/>-]*[A-Z0-9] \[[^]]*\]/ }
function nfailed(s) { return s ~ /^ +[A-Z0-9 -]*(FAIL|FLKY|FLAKY|SIG|ABORT|TIMEOUT|ERR|CRASH|EXIT)/ }
function ntest(s) {
  sub(/^ +[^[]*\[[^]]*\] /, "", s)
  sub(/^\[[0-9]+\/[0-9]+\] /, "", s)
  sub(/^\([0-9]+\/[0-9]+\) /, "", s)
  return s
}
BEGIN {
  kind = ENVIRON["BRIEF_KIND"]; logpath = ENVIRON["BRIEF_LOG"]
  status = ENVIRON["BRIEF_STATUS"] + 0
  maxl = ENVIRON["BRIEF_MAX_LINES"] + 0; maxb = ENVIRON["BRIEF_MAX_BYTES"] + 0
  esc = sprintf("%c", 27)
}
{
  line = $0
  sub(/\r$/, "", line)
  gsub(esc "\\[[0-9;?]*[A-Za-z]", "", line)
  gsub(esc "\\([A-Z0-9]", "", line)
  nlines++; nbytes += length($0) + 1
  tail[nlines % 40] = line

  if (kind == "fmt") {
    detail(line)
    if (line ~ /^Diff in /) { diffs++; f = line; sub(/^Diff in /, "", f); sub(/(:[0-9]+:| at line [0-9]+:)$/, "", f); if (!(f in files)) { files[f] = 1; nfiles++ } }
    next
  }
  if (line ~ /^ +Finished /) { finished = line; sub(/^ +/, "", finished) }

  # After nextest'"'"'s Summary everything is the summary; a later Summary (a
  # stress run'"'"'s last) replaces it.
  if (line ~ /^ +Summary \[/) { ns = 0; summary(line); aftersum = 1; next }
  if (aftersum) { summary(line); next }

  # A diagnostic ends at a blank line or at the next progress line of cargo;
  # a "Caused by:" block (a build script'"'"'s --- stdout and --- stderr) only
  # at a line that is neither blank nor indented.
  if (indiag) {
    if (line ~ /^ +(Compiling|Checking|Finished|Running|Doc-tests|Fresh|Blocking|Building|Download(ed|ing)|Updating|Locking|Adding) /) { indiag = 0; caused = 0 }
    else if (caused) {
      if (line == "" || line ~ /^( |---|Caused by:)/) { detail(line); next }
      indiag = 0; caused = 0
    }
    else if (line == "") { detail(""); indiag = 0; next }
    else if (line !~ /^(error|warning)(\[[A-Za-z0-9]+\])?(: |:$)/) { detail(line); next }
    else indiag = 0
  }
  warn = 0
  if (inblock) {
    if (line ~ /^failures:$/) { inblock = 0 }
    else { detail(line); next }
  }
  if (innext) {
    if (nstatus(line) || line ~ /^ *(Cancelling|Summary \[|Stress test|Starting )/ || line ~ /^(\342\224\200|error)/) innext = 0
    else {
      # libtest'"'"'s own lines in the captured stdout say nothing new.
      if (!skipnext && line !~ /^    (running [0-9]+ tests?|test .* \.\.\. [A-Za-z]+|failures:|test result: .*)$/) detail(line)
      next
    }
  }
  if (pendfail != "") {
    if (line ~ /^    [^ ]/) { detail(pendfail); pendfail = ""; inlist = 1 }
    else pendfail = ""
  }
  if (inlist) {
    if (line ~ /^    [^ ]/) { detail(line); next }
    inlist = 0; detail("")
  }

  if (line ~ /^---- .* ----$/) { detail(line); inblock = 1; next }
  if (line ~ /^failures:$/) { pendfail = line; next }
  if (line ~ /^test result: /) { summary(line); next }
  if (nstatus(line)) {
    if (nfailed(line)) {
      detail(line); t = ntest(line)
      # A stress run repeats a test'"'"'s output in each iteration; a retry'"'"'s
      # output may differ, so it is kept.
      skipnext = (t in seen) && line ~ /^ +[^[]*\[[^]]*\] \[[0-9]+\/[0-9]+\] /
      seen[t] = 1; innext = 1
    }
    next
  }
  if (line ~ /^(error|warning)(\[[A-Za-z0-9]+\])?(: |:$)/ || line ~ /^Caused by:$/ || line ~ /^thread .* panicked/) {
    warn = line ~ /^warning/; caused = line ~ /^Caused by:$/
    detail(line); indiag = 1; next
  }
}
END {
  if (kind == "fmt") {
    if (diffs > 0) summary(sprintf("cargo fmt: %d diff(s) in %d file(s)", diffs, nfiles))
    else if (status == 0) summary("cargo fmt: no diff")
  } else if (ns == 0 && finished != "") summary(finished)
  warn = 0
  if (status != 0 && nd == 0) {
    from = nlines > 40 ? nlines - 39 : 1
    for (i = from; i <= nlines; i++) detail(tail[i % 40])
  }
  while (nd > 0 && d[nd] == "") nd--
  if (nd > 0 && nw > 0) detail("")
  for (i = 1; i <= nw; i++) detail(w[i])
  while (nd > 0 && d[nd] == "") nd--

  lines = 0; bytes = 0
  for (i = 1; i <= nd; i++) {
    if (lines + 1 > maxl || bytes + length(d[i]) + 1 > maxb) break
    print d[i]; lines++; bytes += length(d[i]) + 1
  }
  if (i <= nd) {
    ol = 0; ob = 0
    for (; i <= nd; i++) { ol++; ob += length(d[i]) + 1 }
    printf "%s: omitted %d more line(s) (%d bytes) of failures and diagnostics; read them in %s\n", "cargo-brief", ol, ob, logpath
  }
  from = ns > 20 ? ns - 19 : 1
  if (from > 1) printf "%s: omitted %d earlier summary line(s)\n", "cargo-brief", from - 1
  for (i = from; i <= ns; i++) print sm[i]
  printf "%s: exit %d; full log (%d lines, %d bytes): %s\n", "cargo-brief", status, nlines, nbytes, logpath
}' < "$2"
}

# self_test runs the script on each recorded output (a fixture replayed by a
# command that prints it and exits with the recorded status) and compares
# what it prints, its exit status and the log it keeps.
self_test() {
  fixtures=$here/cargo-brief-fixtures
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  fail=0
  esc=$(printf '\033')
  # "case status max_lines max_bytes command": the fixture is <case>.log (\033 in it
  # stands for the escape byte), the expected output <case>.out with LOG for
  # the log's path.
  while read -r name want maxl maxb cmd; do
    [ -n "$name" ] || continue
    sed "s/\\\\033/$esc/g" "$fixtures/$name.log" > "$tmp/$name.fixture"
    set +e
    # shellcheck disable=SC2086 # cmd is a list of words
    CARGO_BRIEF_MAX_LINES=$maxl CARGO_BRIEF_MAX_BYTES=$maxb sh "$script" --log "$tmp/$name.kept" \
      sh -c 'cat "$1"; exit "$2"' replay "$tmp/$name.fixture" "$want" $cmd > "$tmp/$name.got" 2>&1
    status=$?
    set -e
    sed "s|$tmp/$name.kept|LOG|g" "$tmp/$name.got" > "$tmp/$name.cmp"
    if [ "$status" -ne "$want" ]; then
      echo "$me --self-test: $name: exit $status, want $want" >&2; fail=1
    elif ! cmp -s "$tmp/$name.fixture" "$tmp/$name.kept"; then
      echo "$me --self-test: $name: the log is not the whole output" >&2; fail=1
    elif ! diff -u "$fixtures/$name.out" "$tmp/$name.cmp" >&2; then
      echo "$me --self-test: $name: output differs from $name.out" >&2; fail=1
    else
      echo "$me --self-test: $name: ok"
    fi
  done <<'EOF'
test-pass 0 150 12000 cargo test
test-fail 101 150 12000 cargo test
test-fail-capped 101 5 12000 cargo test
test-fail-bytes 101 150 200 cargo test
test-compile-error 101 150 12000 cargo test
nextest-pass 0 150 12000 cargo nextest run
nextest-fail 100 150 12000 cargo nextest run
nextest-stress-pass 0 150 12000 cargo nextest run --stress-count 3
nextest-stress-fail 100 150 12000 cargo nextest run --stress-count 3
clippy-pass 0 150 12000 cargo clippy
clippy-warning 0 150 12000 cargo clippy
clippy-deny 101 150 12000 cargo clippy -- -D warnings
fmt-pass 0 150 12000 cargo fmt --check
fmt-diff 1 150 12000 cargo fmt --check
crash 101 150 12000 cargo test
build-script 101 150 12000 cargo test
test-warnings-fail 101 16 12000 cargo test
nextest-retry 100 150 12000 cargo nextest run --retries 1
wrapper-fail 1 150 12000 sh scripts/landing-it.sh
EOF
  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

if [ "${1:-}" = "--self-test" ]; then
  [ $# -eq 1 ] || usage "--self-test takes no other argument"
  self_test
  exit 0
fi

log=""
while [ $# -gt 0 ]; do
  case "$1" in
    --log) [ $# -ge 2 ] || usage "--log needs a file"; log=$2; shift 2 ;;
    -h|--help) sed -n '2,47p' "$0"; exit 0 ;;
    --) shift; break ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || usage "no command"

if [ -z "$log" ]; then
  dir=${TMPDIR:-/tmp}
  log=$(mktemp "${dir%/}/$me.XXXXXX") || { echo "$me: cannot make a log under $dir" >&2; exit 2; }
fi
: > "$log" 2>/dev/null || { echo "$me: cannot write $log" >&2; exit 2; }

set +e
"$@" > "$log" 2>&1
status=$?
# The command's status wins even when the shortening fails.
brief "$(kind_of "$@")" "$log" "$status" || echo "$me: could not shorten the output; full log: $log" >&2
exit "$status"
