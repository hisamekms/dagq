#!/bin/sh
# The map from the repository's files to the integration tests that run them
# (goal 157, stage 2, "method B"): CI's nightly job
# (.github/workflows/it-coverage-map.yml) runs every test of the integration
# test binaries with coverage, one profile per test, and writes for each
# repository file the tests whose run covered at least one of its lines, with
# each test's time. The landing's narrowed IT (stage 3b) and the measurement
# on past landings (stage 3a) read the latest table with gh run download.
# The record is listed in docs/design/measurement.md, section "SSOTとビュー".
#
# Usage: sh scripts/it-coverage-map.sh --run OUT.json
#        sh scripts/it-coverage-map.sh --runner BINARY [ARGS...]
#        sh scripts/it-coverage-map.sh --self-test
#   --run OUT.json  build the workspace with coverage (cargo llvm-cov
#                   show-env), run the tests, export each test's coverage and
#                   write the table to OUT.json. Needs cargo-llvm-cov,
#                   cargo-nextest, the llvm-tools-preview component and
#                   python3. The work directory is $IT_COVERAGE_WORK (default
#                   target/it-coverage), removed at the start.
#   --runner        the target runner nextest calls for each test binary
#                   (CARGO_TARGET_<TRIPLE>_RUNNER, set by --run): in the run
#                   phase it points LLVM_PROFILE_FILE at the test's own
#                   directory, then runs the binary
#   --self-test     check that the table is assembled from the fixtures under
#                   scripts/it-coverage-map-fixtures/ (a nextest list, a JUnit
#                   file and per-test llvm-cov export JSON) as expected.json
#                   says, and the runner's choice of directory with a fake
#                   test binary. Needs only sh and python3.
#
# Which tests: the nextest filterset "kind(test) - binary(e2e)" over the
# workspace (--workspace), i.e. the integration test binaries: tests/it
# (dagq::it) and tests/plugin.rs (dagq::plugin). Unit tests (kind lib, run in full at every
# landing) and tests/e2e.rs (needs cmux, #[ignore]d) are out.
#
# How (one profile per test): nextest runs each test in its own process and
# calls the target runner with NEXTEST_BINARY_ID and NEXTEST_TEST_NAME set.
# The runner sets LLVM_PROFILE_FILE=<work>/tests/<binary id, / as @>/<test
# name>/%1m.profraw, so the test and every dagq process it spawns (they
# inherit the variable) merge their counters into that directory. Then, for
# each test, llvm-profdata merge and llvm-cov export -summary-only (objects:
# the test binary and the non-test binaries of the nextest list) give the
# files with a covered line. The tests run in $IT_COVERAGE_PARTITIONS
# (default 8) hash partitions, each exported (and its profraw files removed)
# before the next, so the profraw files of one partition at most are on disk.
# The nextest profile it-coverage (.config/nextest.toml) writes the JUnit file.
#
# Limits: a process a test spawns with a cleared environment does not get
# LLVM_PROFILE_FILE, and one killed by SIGKILL (or still running when the
# test's partition is exported) writes no profile in time, so the files only
# such a process runs are missed for that test. A
# file reached only through a macro or generic code instantiated in another
# crate counts where llvm-cov puts it.
#
# The table (JSON, OUT.json; the artifact's it-coverage-map.json):
#   {
#    "version": 1,                      # bumped when a key changes meaning
#    "commit": "<full SHA the table was built from>",
#    "generated_at": "2026-10-06T18:42:10Z",   # UTC, when the table was written
#    "run_url": "https://github.com/.../actions/runs/N" or null,
#    "filter": "kind(test) - binary(e2e)",
#    "tests": {
#     "dagq::it::runtime_claim::claims_the_ready_task": {
#      "binary_id": "dagq::it",         # nextest's binary id
#      "name": "runtime_claim::claims_the_ready_task",  # nextest's test name
#      "duration_secs": 1.234,          # nextest JUnit time (with coverage,
#                                       # slower than a plain run); null if
#                                       # the test did not run
#      "status": "passed",              # passed, failed, flaky (failed, then
#                                       # passed on the retry) or not_run
#      "coverage": true,                # false: no profile was exported
#      "files": 42                      # number of files it covered
#     }, ...
#    },
#    "files": {
#     "src/app/claim.rs": ["dagq::it::runtime_claim::claims_the_ready_task", ...],
#     ...
#    }
#   }
# A test's key is nextest's binary id and test name joined by "::"
# (<binary id>::<test name>, e.g. dagq::it::runtime_claim::x); to build a
# nextest filter take binary_id and name from "tests" (binary_id(dagq::it) &
# test(=runtime_claim::x)), not by splitting the key. "files" has paths
# relative to the repository root with /, sorted, every file of the
# repository (src/, tests/, ...) outside target/ that a test covered
# at least one line of; each list is sorted. A file no test covers is absent.
# A failed test keeps the files it covered. Coverage is per file, not per
# line or function.
#
# Where: the GitHub Actions artifact "it-coverage-map" (the one file
# it-coverage-map.json) of the workflow "IT coverage map"
# (.github/workflows/it-coverage-map.yml), kept 30 days. It runs daily at
# 19:00 UTC, on workflow_dispatch, and on a push to main that changes the
# workflow, this script, it-coverage-map.py or the fixtures. The latest:
#   gh run list --workflow it-coverage-map.yml --branch main --status success \
#     --limit 1 --json databaseId --jq '.[0].databaseId'
#   gh run download <id> --name it-coverage-map
#
# Time (an estimate, not yet measured): the build with coverage 10-20 min,
# the tests about as long as CI's cargo llvm-cov nextest step (the IT take
# about 3,000 s of test time, on the runner's 3 CPUs 20-30 min), the export
# 10-25 min (about 1,100 tests, 1-3 s each, as many at once as CPUs), plus
# each of the 8 partitions waiting for its slowest test: about 45-75 min in
# all. The job's timeout-minutes is 150. The first runs' times
# replace this estimate.
#
# Exit status: 0 when the table is written (failed tests included: they are
# in "tests" with status failed), 1 when the self-test fails or the table
# cannot be built (also when no test got coverage: a build without coverage
# or a broken runner), 2 on a usage error. The repository's path must have no
# spaces (the runner's command and the --junit options are split on them).
set -eu

me=it-coverage-map
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")
py=$here/it-coverage-map.py
filter='kind(test) - binary(e2e)'

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/it-coverage-map.sh --run OUT.json | --runner BINARY [ARGS...] | --self-test" >&2
  exit 2
}

# runner: the target runner. Outside nextest's run phase (the --list calls)
# the profiles go to <work>/other and are not read.
runner() {
  [ $# -ge 1 ] || usage "--runner needs the test binary"
  name=${NEXTEST_TEST_NAME:-}
  if [ -z "$name" ] && [ "${NEXTEST_TEST_PHASE:-run}" = run ]; then
    # An older nextest without NEXTEST_TEST_NAME: the argument after --exact.
    prev=""
    for arg in "$@"; do
      [ "$prev" = --exact ] && name=$arg
      prev=$arg
    done
  fi
  if [ -n "${IT_COVERAGE_TESTS_DIR:-}" ] && [ "${NEXTEST_TEST_PHASE:-run}" = run ] && [ -n "$name" ]; then
    binary_id=${NEXTEST_BINARY_ID:-$(basename "$1")}
    dir=$IT_COVERAGE_TESTS_DIR/$(printf '%s' "$binary_id" | tr / @)/$name
    mkdir -p "$dir"
    [ -f "$dir/test.txt" ] || printf '%s\n%s\n' "$binary_id" "$name" > "$dir/test.txt"
    [ -f "$dir/binary.txt" ] || printf '%s\n' "$1" > "$dir/binary.txt"
    LLVM_PROFILE_FILE=$dir/%1m.profraw
    export LLVM_PROFILE_FILE
  elif [ -n "${IT_COVERAGE_OTHER_DIR:-}" ]; then
    LLVM_PROFILE_FILE=$IT_COVERAGE_OTHER_DIR/%p-%1m.profraw
    export LLVM_PROFILE_FILE
  fi
  exec "$@"
}

run() {
  out=$1
  root=$(git rev-parse --show-toplevel)
  commit=$(git -C "$root" rev-parse HEAD)
  work=${IT_COVERAGE_WORK:-$root/target/it-coverage}
  case $work in /*) ;; *) work=$(pwd)/$work ;; esac
  partitions=${IT_COVERAGE_PARTITIONS:-8}
  case $partitions in '' | *[!0-9]* | 0) usage "IT_COVERAGE_PARTITIONS is not a positive number: $partitions" ;; esac
  rm -rf "$work"
  mkdir -p "$work/tests" "$work/other"
  cd "$root"

  # The instrumented build's environment (RUSTFLAGS through cargo-llvm-cov's
  # wrapper, LLVM_PROFILE_FILE for anything the runner does not set).
  # The wrapper's flags are not in cargo's fingerprint, so a workspace crate
  # built before without coverage would be reused: clean the workspace's
  # artifacts (not the dependencies') first, as cargo-llvm-cov's README says.
  # (Captured first: eval of a failed substitution would hide the failure.)
  envs=$(cargo llvm-cov show-env --export-prefix)
  eval "$envs"
  cargo llvm-cov clean --workspace
  LLVM_PROFILE_FILE=$work/other/%p-%1m.profraw
  host=$(rustc -vV | sed -n 's/^host: //p')
  var=CARGO_TARGET_$(printf '%s' "$host" | tr 'a-z-' 'A-Z_')_RUNNER
  export LLVM_PROFILE_FILE "$var=sh $script --runner"
  IT_COVERAGE_TESTS_DIR=$work/tests
  IT_COVERAGE_OTHER_DIR=$work/other
  export IT_COVERAGE_TESTS_DIR IT_COVERAGE_OTHER_DIR

  cargo nextest list --locked --workspace --message-format json -E "$filter" > "$work/list.json"
  target=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["rust-build-meta"]["target-directory"])' < "$work/list.json")
  junits=""
  i=1
  while [ "$i" -le "$partitions" ]; do
    # A failed test fails nextest; the table still records it (status failed).
    cargo nextest run --locked --workspace --profile it-coverage \
      --partition "hash:$i/$partitions" -E "$filter" || echo "$me: partition $i/$partitions: nextest exited $?" >&2
    if [ -f "$target/nextest/it-coverage/junit.xml" ]; then
      mv "$target/nextest/it-coverage/junit.xml" "$work/junit-$i.xml"
      junits="$junits --junit $work/junit-$i.xml"
    fi
    python3 "$py" export --tests-dir "$work/tests" --list "$work/list.json"
    rm -rf "$work/other"
    mkdir -p "$work/other"
    i=$((i + 1))
  done
  # shellcheck disable=SC2086 # $junits is a list of options without spaces
  python3 "$py" assemble --tests-dir "$work/tests" --list "$work/list.json" \
    --root "$root" --commit "$commit" --out "$out" $junits \
    ${IT_COVERAGE_RUN_URL:+--run-url "$IT_COVERAGE_RUN_URL"}
}

self_test() {
  python3 "$py" self-test "$here/it-coverage-map-fixtures" || {
    echo "$me: self-test: FAILED (the table)" >&2
    exit 1
  }
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  # A fake test binary that prints the LLVM_PROFILE_FILE it was given.
  printf '#!/bin/sh\nprintf "%%s\\n" "$LLVM_PROFILE_FILE"\n' > "$tmp/it-0123"
  chmod +x "$tmp/it-0123"
  fail=0
  check() {
    if [ "$2" != "$3" ]; then
      echo "$me: self-test: $1: got '$2', want '$3'" >&2
      fail=1
    fi
  }
  got=$(IT_COVERAGE_TESTS_DIR=$tmp/tests IT_COVERAGE_OTHER_DIR=$tmp/other \
    NEXTEST_TEST_PHASE=run NEXTEST_BINARY_ID=dagq::bin/x NEXTEST_TEST_NAME=mod_a::one \
    sh "$script" --runner "$tmp/it-0123" --exact mod_a::one --nocapture)
  check "run phase" "$got" "$tmp/tests/dagq::bin@x/mod_a::one/%1m.profraw"
  check "test.txt" "$(cat "$tmp/tests/dagq::bin@x/mod_a::one/test.txt")" "dagq::bin/x
mod_a::one"
  check "binary.txt" "$(cat "$tmp/tests/dagq::bin@x/mod_a::one/binary.txt")" "$tmp/it-0123"
  got=$(env -u NEXTEST_TEST_NAME -u NEXTEST_BINARY_ID IT_COVERAGE_TESTS_DIR=$tmp/tests \
    IT_COVERAGE_OTHER_DIR=$tmp/other NEXTEST_TEST_PHASE=run \
    sh "$script" --runner "$tmp/it-0123" --exact mod_b::two --nocapture)
  check "no NEXTEST_TEST_NAME" "$got" "$tmp/tests/it-0123/mod_b::two/%1m.profraw"
  got=$(IT_COVERAGE_TESTS_DIR=$tmp/tests IT_COVERAGE_OTHER_DIR=$tmp/other \
    NEXTEST_TEST_PHASE=list NEXTEST_BINARY_ID=dagq::it \
    sh "$script" --runner "$tmp/it-0123" --list --format terse)
  check "list phase" "$got" "$tmp/other/%p-%1m.profraw"
  if [ "$fail" -ne 0 ]; then
    echo "$me: self-test: FAILED (the runner)" >&2
    exit 1
  fi
  echo "$me: self-test: ok"
}

[ $# -ge 1 ] || usage "no mode"
case $1 in
  --run)
    [ $# -eq 2 ] || usage "--run needs OUT.json"
    case $2 in /*) out=$2 ;; *) out=$(pwd)/$2 ;; esac
    run "$out"
    ;;
  --runner)
    shift
    runner "$@"
    ;;
  --self-test)
    [ $# -eq 1 ] || usage "--self-test takes no argument"
    self_test
    ;;
  *) usage "unknown mode: $1" ;;
esac
