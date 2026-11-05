#!/bin/sh
# The map from the repository's files to the integration tests that run them
# (goal 157, stage 2, "method B"): CI's hourly Linux jobs
# (.github/workflows/it-coverage-map.yml) run every test of the integration
# test binaries with coverage, one profile per test, split into partitions
# whose results one last job merges. The table has for each repository file
# the tests whose run covered at least one of its lines, with each test's
# time. The landing's narrowed IT (stage 3b) and the measurement on past
# landings (stage 3a) read the latest table with gh run download.
#
# A table built on Linux lacks what only macOS runs: a test that only reaches a
# line through a macOS branch is not tied to that file. The table therefore
# records the OS it was built on and the files with an OS condition
# (ADR-t3926-1); the macOS job of ci.yml stays the final gate for that code.
# The record is listed in docs/design/measurement.md, section "SSOTとビュー".
#
# Usage: sh scripts/it-coverage-map.sh --run OUT.json
#        sh scripts/it-coverage-map.sh --partition K/N OUT.json
#        sh scripts/it-coverage-map.sh --merge N PARTS_DIR OUT.json
#        sh scripts/it-coverage-map.sh --runner BINARY [ARGS...]
#        sh scripts/it-coverage-map.sh --self-test
#   --run OUT.json  build the workspace with coverage (cargo llvm-cov
#                   show-env), run the tests, export each test's coverage and
#                   write the table to OUT.json. Needs cargo-llvm-cov,
#                   cargo-nextest, the llvm-tools-preview component and
#                   python3. The work directory is $IT_COVERAGE_WORK (default
#                   target/it-coverage), removed at the start.
#   --partition K/N OUT.json
#                   the same build, then only partition K of N (nextest's
#                   hash:K/N, the same split on every machine): run it, export
#                   each test's coverage and write the partition's part (below)
#                   to OUT.json. Needs what --run needs. The seconds of the
#                   build, the tests and the export and the free disk go to
#                   stderr and to $GITHUB_STEP_SUMMARY when it is set.
#   --merge N PARTS_DIR OUT.json
#                   merge the N parts (every *.json of PARTS_DIR) into the table
#                   for the commit checked out here. Needs python3 and git, no
#                   Rust tool. It writes no table when a part is missing, when
#                   the parts disagree on N, the commit, the OS or the listed
#                   tests, when a part's commit is not the one checked out, when
#                   no test of a part has coverage, or when the tests that ran
#                   are not the listed tests.
#   --runner        the target runner nextest calls for each test binary
#                   (CARGO_TARGET_<TRIPLE>_RUNNER, set by --run): in the run
#                   phase it points LLVM_PROFILE_FILE at the test's own
#                   directory, then runs the binary
#   --self-test     check that the table is assembled from the fixtures under
#                   scripts/it-coverage-map-fixtures/ (a nextest list, a JUnit
#                   file and per-test llvm-cov export JSON) as expected.json
#                   says, merging parts, finding the files with an OS
#                   condition in a commit, the runner's choice of directory
#                   with a fake test binary, and --partition and --merge with a
#                   fake cargo. Needs only sh, python3 and git.
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
# before the next, so the profraw files of one partition at most are on disk;
# --partition runs one of them, so each CI job holds only its own.
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
#    "platform": "linux",               # the OS it was built on (linux, macos)
#    "platform_gated_files": ["src/infrastructure/launchd.rs", ...],
#                                       # below: code that differs by OS
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
# line or function. "platform_gated_files" has, sorted, the .rs files under src/
# and crates/ in the tree of "commit" with a #[cfg(...)], #![cfg(...)] or
# cfg!(...) (over several lines too) that names target_os or
# target_vendor = "apple"; comments and string literals do not count.
#
# A part (JSON, --partition's OUT.json; the artifact
# it-coverage-map-part-K's it-coverage-map-part-K.json, kept 1 day): what the
# merge needs of a job, with no path of its machine:
#   {
#    "version": 1,
#    "commit": "<full SHA>",
#    "platform": "linux",
#    "partition": {"index": 1, "count": 8},
#    "listed": [["dagq::it", "runtime_claim::x"], ...],  # every partition's
#    "results": [["dagq::it", "runtime_claim::x", 1.234, "passed"], ...],
#    "covered": [["dagq::it", "runtime_claim::x", ["src/app/claim.rs", ...]], ...]
#   }
#
# Where: the GitHub Actions artifact "it-coverage-map" (the one file
# it-coverage-map.json) of the workflow "IT coverage map"
# (.github/workflows/it-coverage-map.yml), kept 3 days. The workflow's on: has
# when it runs. The latest:
#   gh run list --workflow it-coverage-map.yml --branch main --status success \
#     --limit 1 --json databaseId --jq '.[0].databaseId'
#   gh run download <id> --name it-coverage-map
#
# Time (an estimate from the Spike docs/plans/it-coverage-map-linux-spike.md,
# not yet measured): with 8 partitions on the runner's 4 CPUs, one job about
# 17 min (24 min when the cache of the instrumented dependencies misses and the
# tests are slower), the merge about 2 min, a whole run 19-26 min. The build is
# the floor: more partitions shorten only the tests and the export. The job
# summaries of the first runs replace this estimate.
#
# Exit status: --run: 0 when the table is written (failed tests included: they
# are in "tests" with status failed), 1 when the table cannot be built (also
# when no test got coverage: a build without coverage or a broken runner).
# --partition: 0 when the part is written (failed and flaky tests included), not
# 0 when the build fails or nextest wrote no JUnit file. --merge: 0 when the
# table is written, failed and flaky tests included (status failed, flaky); 1
# for the disagreements above. --self-test: 1 when it fails. 2 on a usage error.
# The repository's path must have no spaces (the runner's command and the
# --junit options are split on them).
set -eu

me=it-coverage-map
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")
py=$here/it-coverage-map.py
filter='kind(test) - binary(e2e)'

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/it-coverage-map.sh --run OUT.json | --partition K/N OUT.json | --merge N PARTS_DIR OUT.json | --runner BINARY [ARGS...] | --self-test" >&2
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

# instrument: the empty work directory and the environment of the instrumented
# build, in the repository's root. Sets root, commit and work.
instrument() {
  root=$(git rev-parse --show-toplevel)
  commit=$(git -C "$root" rev-parse HEAD)
  work=${IT_COVERAGE_WORK:-$root/target/it-coverage}
  case $work in /*) ;; *) work=$(pwd)/$work ;; esac
  rm -rf "$work"
  mkdir -p "$work/tests" "$work/other"
  cd "$root"

  # The instrumented build's environment (RUSTFLAGS through cargo-llvm-cov's
  # wrapper, LLVM_PROFILE_FILE for anything the runner does not set).
  # The wrapper's flags are not in cargo's fingerprint, so a workspace crate
  # built before without coverage would be reused: clean the workspace's
  # artifacts (not the dependencies') first, as cargo-llvm-cov's README says.
  # (Captured first: eval of a failed substitution would hide the failure.)
  envs=$(cargo llvm-cov show-env --sh)
  eval "$envs"
  cargo llvm-cov clean --workspace
  LLVM_PROFILE_FILE=$work/other/%p-%1m.profraw
  host=$(rustc -vV | sed -n 's/^host: //p')
  var=CARGO_TARGET_$(printf '%s' "$host" | tr 'a-z-' 'A-Z_')_RUNNER
  export LLVM_PROFILE_FILE "$var=sh $script --runner"
  IT_COVERAGE_TESTS_DIR=$work/tests
  IT_COVERAGE_OTHER_DIR=$work/other
  export IT_COVERAGE_TESTS_DIR IT_COVERAGE_OTHER_DIR
}

run() {
  out=$1
  partitions=${IT_COVERAGE_PARTITIONS:-8}
  case $partitions in '' | *[!0-9]* | 0) usage "IT_COVERAGE_PARTITIONS is not a positive number: $partitions" ;; esac
  instrument

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

free_disk() {
  df -h "$root" | awk 'END { print $4 }'
}

# partition K N OUT: one partition's tests and its part. nextest's list builds
# the test binaries, so its time is the build's and its failure ends the job.
partition() {
  k=$1
  n=$2
  out=$3
  instrument
  disk_start=$(free_disk)
  t0=$(date +%s)
  cargo nextest list --locked --workspace --message-format json -E "$filter" > "$work/list.json"
  t1=$(date +%s)
  disk_built=$(free_disk)
  target=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["rust-build-meta"]["target-directory"])' < "$work/list.json")
  # A failed test fails nextest; the part still records it (status failed). If
  # nextest stops short, the tests it did not reach fail the merge.
  cargo nextest run --locked --workspace --profile it-coverage \
    --partition "hash:$k/$n" -E "$filter" || echo "$me: partition $k/$n: nextest exited $?" >&2
  t2=$(date +%s)
  disk_tested=$(free_disk)
  if [ ! -f "$target/nextest/it-coverage/junit.xml" ]; then
    echo "$me: partition $k/$n: nextest wrote no JUnit file" >&2
    exit 1
  fi
  python3 "$py" export --tests-dir "$work/tests" --list "$work/list.json"
  t3=$(date +%s)
  python3 "$py" part --tests-dir "$work/tests" --list "$work/list.json" \
    --junit "$target/nextest/it-coverage/junit.xml" --root "$root" \
    --commit "$commit" --index "$k" --count "$n" --out "$out"
  report=$(cat <<EOF
## IT coverage partition $k/$n

| phase | seconds | free disk after |
|---|---|---|
| start | | $disk_start |
| build | $((t1 - t0)) | $disk_built |
| IT | $((t2 - t1)) | $disk_tested |
| export | $((t3 - t2)) | $(free_disk) |
EOF
  )
  printf '%s\n' "$report" >&2
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    printf '%s\n' "$report" >> "$GITHUB_STEP_SUMMARY"
  fi
}

merge() {
  root=$(git rev-parse --show-toplevel)
  python3 "$py" merge --parts-dir "$2" --count "$1" --root "$root" \
    --commit "$(git -C "$root" rev-parse HEAD)" --out "$3" \
    ${IT_COVERAGE_RUN_URL:+--run-url "$IT_COVERAGE_RUN_URL"}
}

self_test() {
  # A hook or a landing may run this with GIT_DIR set; the throwaway
  # repositories below must not touch that one.
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
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
  self_test_modes
  if [ "$fail" -ne 0 ]; then
    echo "$me: self-test: FAILED (--partition and --merge)" >&2
    exit 1
  fi
  echo "$me: self-test: ok"
}

# self_test_modes: --partition and --merge end to end in a throwaway repository
# with a fake cargo and rustc: partition 1 has a failed test, partition 2 a flaky
# one, and nextest exits 100 for both. Uses $tmp and check from self_test.
self_test_modes() {
  fx=$here/it-coverage-map-fixtures
  repo=$tmp/repo
  mkdir -p "$repo/src" "$tmp/bin" "$tmp/parts"
  printf '#[cfg(target_os = "macos")]\nfn f() {}\n' > "$repo/src/gated.rs"
  git -C "$repo" init -q
  git -C "$repo" add .
  git -C "$repo" -c user.name=self-test -c user.email=self-test@example.invalid \
    -c core.hooksPath=/dev/null commit -q -m fixture
  sed "s|/work/dagq|$repo|g" "$fx/list.json" > "$tmp/list.json"
  python3 - "$tmp/list.json" <<'EOF'
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    listing = json.load(f)
del listing["rust-suites"]["dagq::it"]["testcases"]["cli_read::did_not_run"]
with open(sys.argv[1], "w", encoding="utf-8") as f:
    json.dump(listing, f)
EOF
  printf '#!/bin/sh\necho "host: x86_64-unknown-linux-gnu"\n' > "$tmp/bin/rustc"
  cat > "$tmp/bin/cargo" <<EOF
#!/bin/sh
# The fake's calls: llvm-cov show-env|clean, nextest list|run.
case "\$1 \$2" in
  "llvm-cov show-env") [ "\$3" = --sh ] || exit 9; echo 'export FAKE_COVERAGE=1' ;;
  "llvm-cov clean") ;;
  "nextest list")
    [ -z "\${FAKE_BUILD_FAILS:-}" ] || exit 101
    cat "$tmp/list.json" ;;
  "nextest run")
    case "\$*" in
      *hash:1/2*) k=1; dirs="it/claims-the-ready-task it/reads-the-status" ;;
      *hash:2/2*) k=2; dirs="broker-serve/starts-and-stops" ;;
      *) exit 9 ;;
    esac
    for d in \$dirs; do
      mkdir -p "\$IT_COVERAGE_TESTS_DIR/\$d"
      cp "$fx/tests/\$d/test.txt" "$fx/tests/\$d/binary.txt" "\$IT_COVERAGE_TESTS_DIR/\$d/"
      sed "s|/work/dagq|$repo|g" "$fx/tests/\$d/export.json" > "\$IT_COVERAGE_TESTS_DIR/\$d/export.json"
    done
    mkdir -p "$repo/target/nextest/it-coverage"
    cp "$fx/junit-part-\$k.xml" "$repo/target/nextest/it-coverage/junit.xml"
    exit 100 ;;
  *) exit 9 ;;
esac
EOF
  chmod +x "$tmp/bin/cargo" "$tmp/bin/rustc"
  in_repo() {
    (cd "$repo" && PATH=$tmp/bin:$PATH IT_COVERAGE_WORK=$tmp/work LLVM_COV=unused LLVM_PROFDATA=unused "$@")
  }
  status=0
  in_repo env GITHUB_STEP_SUMMARY="$tmp/summary.md" sh "$script" --partition 1/2 "$tmp/parts/1.json" 2> "$tmp/err" || status=$?
  check "--partition exit status with a failed test" "$status" 0
  grep -q '^| IT | [0-9]* | ' "$tmp/summary.md" 2> /dev/null || {
    echo "$me: self-test: no job summary: $(cat "$tmp/err")" >&2
    fail=1
  }
  status=0
  in_repo env FAKE_BUILD_FAILS=1 sh "$script" --partition 2/2 "$tmp/parts/broken.json" 2> /dev/null || status=$?
  check "--partition exit status with a failed build" "$status" 101
  check "--partition part with a failed build" "$(ls "$tmp/parts" | grep -c broken || :)" 0
  status=0
  in_repo sh "$script" --merge 2 "$tmp/parts" "$tmp/missing.json" 2> /dev/null || status=$?
  check "--merge exit status with a missing part" "$status" 1
  check "--merge table with a missing part" "$(ls "$tmp" | grep -c missing.json || :)" 0
  status=0
  in_repo sh "$script" --partition 2/2 "$tmp/parts/2.json" 2> "$tmp/err" || status=$?
  check "--partition exit status with a flaky test" "$status" 0
  status=0
  in_repo sh "$script" --merge 2 "$tmp/parts" "$tmp/map.json" 2> "$tmp/err" || status=$?
  check "--merge exit status with a failed test" "$status" 0
  got=$(python3 - "$tmp/map.json" <<'EOF' 2> /dev/null || :
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    t = json.load(f)
print(t["platform_gated_files"], sorted(v["status"] for v in t["tests"].values()),
      len(t["files"]))
EOF
  )
  check "--merge table" "$got" "['src/gated.rs'] ['failed', 'flaky', 'passed'] 5"
}

[ $# -ge 1 ] || usage "no mode"
case $1 in
  --run)
    [ $# -eq 2 ] || usage "--run needs OUT.json"
    case $2 in /*) out=$2 ;; *) out=$(pwd)/$2 ;; esac
    run "$out"
    ;;
  --partition)
    [ $# -eq 3 ] || usage "--partition needs K/N and OUT.json"
    case $2 in *[!0-9/]* | */*/*) usage "--partition: not K/N: $2" ;; [1-9]*/[1-9]*) ;; *) usage "--partition: not K/N: $2" ;; esac
    k=${2%%/*}
    n=${2#*/}
    { [ "$k" -ge 1 ] && [ "$k" -le "$n" ]; } || usage "--partition: not K/N with 1 <= K <= N: $2"
    case $3 in /*) out=$3 ;; *) out=$(pwd)/$3 ;; esac
    partition "$k" "$n" "$out"
    ;;
  --merge)
    [ $# -eq 4 ] || usage "--merge needs N, PARTS_DIR and OUT.json"
    case $2 in '' | *[!0-9]* | 0) usage "--merge: N is not a positive number: $2" ;; esac
    merge "$2" "$3" "$4"
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
