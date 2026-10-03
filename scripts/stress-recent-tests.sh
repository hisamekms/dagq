#!/bin/sh
# Repeat the #[test] functions added or changed in a range of commits under
# cargo nextest's stress mode (ADR-t920-1 decision 2: the heavy repetition
# the worker no longer does runs once a day in GitHub Actions).
#
# The tests are chosen as docs/development/local-checks.md, section "stress", chooses them: the
# #[test] functions whose lines the diff adds or changes in tests/it/** (the
# `it` binary, named <module>::<name>), in src/ (the dagq lib and bin,
# named <module path>::tests::<name>), and in crates/<crate>/src and
# crates/<crate>/tests. tests/e2e.rs and tests/plugin.rs are left out.
#
# Usage: sh scripts/stress-recent-tests.sh [--base REV] [--since DATE]
#                                          [--head REV] [--count N]
#                                          [--list]
#   --base REV    the range starts at REV (exclusive)
#   --since DATE  without --base, the range starts at the last first-parent
#                 commit of HEAD older than DATE (git's --before; the default
#                 is "24 hours ago")
#   --head REV    the range ends at REV (default HEAD; check it out first)
#   --count N     stress iterations (default 20)
#   --list        print the chosen tests and stop without running them
# The same settings come from STRESS_BASE, STRESS_SINCE, STRESS_HEAD and
# STRESS_COUNT. STRESS_DURATION (for example 30m) replaces the count with a
# time limit. STRESS_TEST_THREADS is nextest's --test-threads (default twice
# the CPUs) and STRESS_JOBS the number of nextest processes run side by side
# (default 2), so the chosen tests compete for the CPU. STRESS_OUT is the
# directory of the logs and of failed-tests.txt (default target/stress).
#
# Exit status: 0 when every iteration passed or there was nothing to run,
# 1 when a test failed (failed-tests.txt lists "<binary id> <test name>" per
# line), 2 on a usage or setup error.
set -eu

base=${STRESS_BASE:-}
since=${STRESS_SINCE:-}
head=${STRESS_HEAD:-HEAD}
count=${STRESS_COUNT:-20}
duration=${STRESS_DURATION:-}
out=${STRESS_OUT:-target/stress}
list_only=0

while [ $# -gt 0 ]; do
  case "$1" in
    --base) base=$2; shift 2 ;;
    --since) since=$2; shift 2 ;;
    --head) head=$2; shift 2 ;;
    --count) count=$2; shift 2 ;;
    --list) list_only=1; shift ;;
    -h|--help) sed -n '2,33p' "$0"; exit 0 ;;
    *) echo "stress-recent-tests: unknown argument: $1" >&2; exit 2 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"

head_sha=$(git rev-parse --verify --quiet "$head^{commit}") || {
  echo "stress-recent-tests: no such commit: $head" >&2
  exit 2
}
if [ -n "$base" ]; then
  base_sha=$(git rev-parse --verify --quiet "$base^{commit}") || {
    echo "stress-recent-tests: no such commit: $base" >&2
    exit 2
  }
else
  since=${since:-24 hours ago}
  base_sha=$(git rev-list -1 --first-parent --before="$since" "$head_sha")
  if [ -z "$base_sha" ]; then
    # The whole history is newer than the window: start at the root.
    base_sha=$(git rev-list --max-parents=0 "$head_sha" | tail -n 1)
  fi
fi

echo "stress-recent-tests: range $base_sha..$head_sha"

# Prints "<binary id>|<module prefix>" for a source file, or nothing when the
# file's tests are not stressed.
target_of() {
  case "$1" in
    tests/e2e.rs|tests/e2e/*|tests/plugin.rs) return 0 ;;
    tests/it/main.rs) echo "dagq::it|" ;;
    tests/it/*.rs)
      m=${1#tests/it/}; m=${m%.rs}; m=${m%/mod}
      echo "dagq::it|$(echo "$m" | sed 's#/#::#g')::" ;;
    src/main.rs) echo "dagq::bin/dagq|" ;;
    src/*.rs) echo "dagq|$(module_of "${1#src/}")" ;;
    crates/*/src/main.rs)
      c=${1#crates/}; c=${c%%/*}
      echo "$c::bin/$c|" ;;
    crates/*/src/*.rs)
      c=${1#crates/}; c=${c%%/*}
      echo "$c|$(module_of "${1#crates/"$c"/src/}")" ;;
    crates/*/tests/*/main.rs)
      c=${1#crates/}; c=${c%%/*}
      b=${1#crates/"$c"/tests/}; b=${b%%/*}
      echo "$c::$b|" ;;
    crates/*/tests/*/*.rs)
      c=${1#crates/}; c=${c%%/*}
      r=${1#crates/"$c"/tests/}; b=${r%%/*}
      m=${r#"$b"/}; m=${m%.rs}; m=${m%/mod}
      echo "$c::$b|$(echo "$m" | sed 's#/#::#g')::" ;;
    crates/*/tests/*.rs)
      c=${1#crates/}; c=${c%%/*}
      b=${1#crates/"$c"/tests/}; b=${b%.rs}
      echo "$c::$b|" ;;
  esac
}

# src/lib.rs -> "", src/a/mod.rs -> "a::", src/a/b.rs -> "a::b::".
module_of() {
  m=${1%.rs}
  case "$m" in lib|main) return 0 ;; esac
  m=${m%/mod}
  echo "$(echo "$m" | sed 's#/#::#g')::"
}

# Reads a Rust file on stdin and prints the name (with its inline module path,
# for example tests::name) of each #[test] function whose lines, from the
# attribute to the closing brace, meet the changed line ranges in `ranges`
# ("a-b a-b ..."). Braces are counted after taking out strings (also the
# ones that span lines, and raw strings), char literals and comments.
changed_tests_awk='
BEGIN {
  n = split(ranges, rs, " ")
  for (i = 1; i <= n; i++) { split(rs[i], ab, "-"); lo[i] = ab[1] + 0; hi[i] = ab[2] + 0 }
  depth = 0; mods = 0; pending = 0; intest = 0
}
function hit(a, b,   i) {
  for (i = 1; i <= n; i++) if (lo[i] <= b && hi[i] >= a) return 1
  return 0
}
# The code of a line without strings, char literals and comments. The state
# (instr: 0 code, 1 string, 2 raw string with rawhash #s; incomment) carries
# over to the next line.
function code(src,   out, i, n, ch, nx, k, h) {
  out = ""; n = length(src); i = 1
  while (i <= n) {
    ch = substr(src, i, 1); nx = substr(src, i + 1, 1)
    if (incomment) {
      if (ch == "*" && nx == "/") { incomment = 0; i += 2 } else i++
      continue
    }
    if (instr == 1) {
      if (ch == "\\") i += 2
      else { if (ch == "\"") { instr = 0; out = out "\"\"" } i++ }
      continue
    }
    if (instr == 2) {
      if (ch == "\"" && substr(src, i + 1, rawhash) == hashes(rawhash)) {
        instr = 0; i += 1 + rawhash; out = out "\"\""
      } else i++
      continue
    }
    if (ch == "/" && nx == "/") break
    if (ch == "/" && nx == "*") { incomment = 1; i += 2; continue }
    if (ch == "\"") { instr = 1; i++; continue }
    if (ch == "r" && (nx == "\"" || nx == "#") && (i == 1 || substr(src, i - 1, 1) !~ /[A-Za-z0-9_]/)) {
      k = i + 1; h = 0
      while (substr(src, k, 1) == "#") { h++; k++ }
      if (substr(src, k, 1) == "\"") { instr = 2; rawhash = h; i = k + 1; continue }
    }
    if (ch == "'\''") {
      if (nx == "\\" && match(substr(src, i), /^'\''\\[^'\'']*'\''/)) { out = out "_"; i += RLENGTH; continue }
      if (substr(src, i + 2, 1) == "'\''") { out = out "_"; i += 3; continue }
    }
    out = out ch; i++
  }
  return out
}
function hashes(k,   r) { r = ""; while (k-- > 0) r = r "#"; return r }
function prefix(   i, p) {
  p = ""
  for (i = 1; i <= mods; i++) p = p modname[i] "::"
  return p
}
{
  line = code($0)
  before = depth
  if (!intest && line ~ /#\[test\]/) { pending = 1; start = NR }
  if (!intest && line ~ /^[ \t]*(pub(\([a-z:]+\))?[ \t]+)?mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*\{/) {
    name = line
    sub(/^[ \t]*(pub(\([a-z:]+\))?[ \t]+)?mod[ \t]+/, "", name)
    sub(/[^A-Za-z0-9_].*/, "", name)
    mods++; modname[mods] = name; moddepth[mods] = before
  }
  if (pending && match(" " line, /[^A-Za-z0-9_]fn[ \t]+[A-Za-z_]/)) {
    name = substr(" " line, RSTART + 3)
    sub(/^[ \t]+/, "", name)
    sub(/[^A-Za-z0-9_].*/, "", name)
    test = prefix() name; fndepth = before; intest = 1; opened = 0; pending = 0
  }
  o = gsub(/\{/, "{", line); c = gsub(/\}/, "}", line)
  depth += o - c
  if (intest && o > 0) opened = 1
  if (intest && opened && depth <= fndepth) {
    if (hit(start, NR)) print test
    intest = 0
  }
  while (mods > 0 && depth <= moddepth[mods]) mods--
}
'

tests_file=$(mktemp)
trap 'rm -f "$tests_file"' EXIT

git diff --name-only --diff-filter=AMR "$base_sha" "$head_sha" -- \
  'src/*.rs' 'tests/it/*.rs' 'crates/*.rs' |
while IFS= read -r path; do
  target=$(target_of "$path")
  [ -n "$target" ] || continue
  binary=${target%%|*}
  prefix=${target#*|}
  # The changed lines on the new side; a pure deletion after line c marks c
  # and c+1 so the test that lost the lines is still chosen.
  ranges=$(git diff -U0 --no-color "$base_sha" "$head_sha" -- "$path" |
    sed -n 's/^@@ -[0-9,]* +\([0-9][0-9]*\)\(,\([0-9][0-9]*\)\)\{0,1\} @@.*/\1 \3/p' |
    awk '{ c = ($2 == "") ? 1 : $2 + 0
           if (c == 0) printf "%d-%d ", $1, $1 + 1
           else printf "%d-%d ", $1, $1 + c - 1 }')
  [ -n "$ranges" ] || continue
  git show "$head_sha:$path" |
    awk -v ranges="$ranges" "$changed_tests_awk" |
    while IFS= read -r name; do
      echo "$binary $prefix$name"
    done
done | sort -u > "$tests_file"

total=$(wc -l < "$tests_file" | tr -d ' ')
if [ "$total" -eq 0 ]; then
  echo "stress-recent-tests: no #[test] added or changed in the range; nothing to run"
  exit 0
fi

echo "stress-recent-tests: $total test(s):"
sed 's/^/  /' "$tests_file"
[ "$list_only" -eq 0 ] || exit 0

filter=$(awk '{ printf "%s(binary_id(%s) & test(=%s))", (NR > 1 ? " | " : ""), $1, $2 }' "$tests_file")

cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
threads=${STRESS_TEST_THREADS:-$((cpus * 2))}
jobs=${STRESS_JOBS:-2}
if [ -n "$duration" ]; then
  stress="--stress-duration $duration"
else
  stress="--stress-count $count"
fi

mkdir -p "$out"
rm -f "$out"/stress-*.log "$out/failed-tests.txt"

# Build once so the side-by-side runs below only execute the binaries.
cargo nextest run --locked --workspace --no-run

# A test the parser named but nextest does not know (a name it got wrong, or
# a test the checkout does not have) is reported, not silently dropped.
cargo nextest list --locked --workspace --color never --message-format oneline -E "$filter" \
  2>/dev/null | sort -u > "$out/listed-tests.txt" || true
missing=$(comm -23 "$tests_file" "$out/listed-tests.txt")
if [ -n "$missing" ]; then
  echo "stress-recent-tests: warning: nextest does not list these tests:"
  echo "$missing" | sed 's/^/  /'
fi
if [ ! -s "$out/listed-tests.txt" ]; then
  echo "stress-recent-tests: none of the tests are in this checkout; nothing to run"
  exit 0
fi

echo "stress-recent-tests: $jobs job(s) x $stress, --test-threads $threads"
pids=""
i=1
while [ "$i" -le "$jobs" ]; do
  # shellcheck disable=SC2086 # $stress is two words on purpose.
  cargo nextest run --locked --workspace --color never --no-fail-fast --no-tests=warn \
    $stress --test-threads "$threads" -E "$filter" \
    > "$out/stress-$i.log" 2>&1 &
  pids="$pids $!"
  i=$((i + 1))
done

status=0
for pid in $pids; do
  wait "$pid" || status=1
done

i=1
while [ "$i" -le "$jobs" ]; do
  echo "::group::stress job $i" 2>/dev/null || true
  tail -n 60 "$out/stress-$i.log"
  echo "::endgroup::"
  i=$((i + 1))
done

# nextest prints a line per failed attempt and per test that passed on its
# retry, for example
#          FAIL [   0.010s] [2/20] (1/3) dagq::it runtime_abandon::name
#   TRY 1 FL+LK [   0.010s] [2/20] (───) dagq::it runtime_abandon::name
#   FLKY-FL 2/2 [   0.010s] [2/20] (1/3) dagq::it runtime_abandon::name
# The statuses are the explicit set of docs/design/stress-ci.md, the same as
# src/domain/verify_failure.rs and ci.yml's Linux job read: a test that failed
# once and passed on its retry is the unstable test this job is for. LEAK (a
# test that left a process behind and still passed) is not in it: nextest
# counts it as passed, and a leak that fails is FAIL + LEAK, LEAK-FAIL, FL+LK
# or LKFAIL. The last two words are the binary id and the test name, also
# after a status of several words (FAIL + LEAK, ABORT SIG 64, TRY 2 SIG 64).
failed_status='^ *(FAIL|FAIL \+ LEAK|XFAIL|LEAK-FAIL|TIMEOUT|ABORT|SIG(HUP|INT|QUIT|ILL|TRAP|ABRT|FPE|KILL|SEGV|PIPE|ALRM|TERM)|ABORT SIG [0-9]+|TRY [0-9]+ (FAIL|FL\+LK|XFAIL|LKFAIL|TMT|ABORT|HUP|INT|QUIT|ILL|TRAP|ABRT|FPE|KILL|SEGV|PIPE|ALRM|TERM|SIG [0-9]+)|FLKY-FL [0-9]+/[0-9]+|FLAKY [0-9]+/[0-9]+) \['
grep -hE "$failed_status" "$out"/stress-*.log |
  awk '$NF !~ /[])]$/ { print $(NF - 1), $NF }' | sort -u > "$out/failed-tests.txt" || true

if [ "$status" -ne 0 ] || [ -s "$out/failed-tests.txt" ]; then
  echo "stress-recent-tests: failed; the tests that failed at least once:"
  sed 's/^/  /' "$out/failed-tests.txt"
  echo "stress-recent-tests: logs in $out/"
  exit 1
fi
echo "stress-recent-tests: every iteration passed"
