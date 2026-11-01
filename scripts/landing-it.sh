#!/bin/sh
# The tests of this repository's landing verification (ADR-t1925-1, goal 157
# stage 3b): every unit test and the integration tests (IT) the diff reaches,
# in two cargo nextest runs: the unit stage first and, only when it passes,
# the IT stage (skipped when no IT is chosen). dagq.toml's [landing_verification] runs this in
# place of a task's coverage gate after the rebase, with the variables the
# runtime hands it (src/domain/landing_verification.rs): DAGQ_LANDING_BASE
# (the main commit the run was rebased onto), DAGQ_CI_KNOWN_FAILURES (the
# JSON of the tests failing on main already) and DAGQ_CI_FIX_RUN (1 when the
# run fixes a CI failure).
#
# Usage: sh scripts/landing-it.sh
#        sh scripts/landing-it.sh --prepare
#        sh scripts/landing-it.sh --self-test
#   --prepare    the preparation of a verification in the container
#                (ADR-t2257-1 decision 4 (b)): on the host, in the main
#                checkout, fetch the table into the cache as below and copy
#                it to $DAGQ_VERIFICATION_MATERIALS/it-coverage-map.json, a
#                directory of the control side's alone that the runtime
#                makes for the verification; the runtime copies it into the
#                run directory's materials, which the verification's
#                container mounts read-only. Without a table nothing is
#                copied, and the choice in the container runs every IT. It
#                runs no cargo and fails only when the materials cannot be
#                written.
#   --self-test  check the choice (the tests chosen, including the IT not in
#                the table, running every IT and why, and the tests left
#                out) on the fixtures under scripts/landing-it-fixtures/
#                (a table, a tree, a nextest list, cases.json and repo/, the
#                tests/common and tests/it the helpers' users are read
#                from), and the hand-over of --prepare's table to a run in
#                the container, and the stages with a stub cargo (a failing
#                unit stage ends the script before the IT stage runs).
#                Needs only sh and python3.
#
# Which IT (scripts/landing-it.py, choose()): over the files of
# git diff $DAGQ_LANDING_BASE..HEAD, the union of
#   - the tests the table maps each file to,
#   - the tests a changed test file defines (tests/it/<m>.rs, tests/plugin.rs,
#     crates/<c>/tests/<t>.rs),
#   - the tests that read a changed file from the repository (plugins/**,
#     dagq.toml and the like, READ_BY_TESTS),
#   - for a changed helper under tests/common, the tests of the tests/it
#     modules (and tests/plugin.rs) that use it, read statically from the
#     checked-out tree: through `common::<helper>`, a name tests/common/mod.rs
#     re-exports from it, another helper that names it, or a tests/it module
#     that names one of those or calls its macros,
#   - the IT nextest lists now that the table does not have (added or renamed
#     after the table was built).
# A file the table maps to too many IT (a broad entry) and a src or crates .rs
# file not in the tree of the table's commit select nothing from the table:
# their other IT are left to CI, and the output names them.
# An old table (old by its age, within the age limit) is still used, and
# the output says so: the files it does not know (a src or crates .rs file
# not in its tree, and one changed on main between the table's commit and
# $DAGQ_LANDING_BASE) are widened to the IT of the table's entries beside
# them (the same directory and the module that declares it), and the output
# names each with the count of IT it added. Broad entries stay left to CI.
# Every IT runs instead, with the reasons printed, when the narrowed IT's
# estimate (widened IT included) is over the threshold, a common file is
# touched, a changed helper is broad (its users hold more than the broad
# entries' share of the IT by #[test] count, as mod.rs and the shared
# fixtures do) or a file under tests/common is no helper's .rs, or the table
# cannot be had or read or is older than the age limit.
# The threshold, the common files, the ages and the share that makes an
# entry or a helper broad are written once, at the top of
# scripts/landing-it.py, with their grounds in
# docs/plans/landing-it-selection.md, section 「決めたこと」. The tests in the
# known failures' "failures" are left out; a fix run's own ones are under
# "kept_for_task" and run, chosen by the diff or not.
#
# The table: the latest it-coverage-map artifact of a successful run of
# .github/workflows/it-coverage-map.yml on main (gh run download; the format
# is at the top of scripts/it-coverage-map.sh). It is kept in
# $LANDING_IT_CACHE (default ${XDG_CACHE_HOME:-$HOME/.cache}/dagq/landing-it),
# outside the queue, as <run id>/it-coverage-map.json with fetched.json (the
# run, its commit, when it was created and when it was fetched), so a landing
# downloads a table only when a newer run is there; the older ones are
# removed. Without gh, or when gh fails, the newest cached table is used. The
# cache is listed in docs/design/measurement.md, section 「SSOTとビュー」.
# In the verification's container (DAGQ_VERIFICATION_MATERIALS set, no gh and
# no network) nothing is fetched: the table is the one --prepare copied into
# the materials, and without one every IT runs.
#
# Exit status: the unit stage's cargo nextest when it fails, else the IT
# stage's (0 when no IT is chosen); 2 when the run cannot be prepared.
set -eu

me=landing-it
root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$(dirname "$0")/.." && pwd)
py="$root/scripts/landing-it.py"

# --self-test's hand-over: --prepare on the host copies the cached table into
# the materials, and a run with the materials (as in the container) reads it
# from there and asks gh nothing. gh and cargo are stubs: no network, no build.
self_test_handover() {
  t=$(mktemp -d "${TMPDIR:-/tmp}/landing-it-self-test.XXXXXX")
  mkdir -p "$t/bin" "$t/cache/1" "$t/empty" "$t/materials"
  printf '#!/bin/sh\n: > "%s/gh-asked"\nexit 1\n' "$t" >"$t/bin/gh"
  printf '#!/bin/sh\necho "cargo $*"\n' >"$t/bin/cargo"
  chmod +x "$t/bin/gh" "$t/bin/cargo"
  fixture="$root/scripts/landing-it-fixtures/table.json"
  cp "$fixture" "$t/cache/1/it-coverage-map.json"
  ok=1
  if ! PATH="$t/bin:$PATH" LANDING_IT_CACHE="$t/cache" DAGQ_VERIFICATION_MATERIALS="$t/materials" \
      sh "$root/scripts/landing-it.sh" --prepare >"$t/prepare" 2>&1 ||
    ! cmp -s "$fixture" "$t/materials/it-coverage-map.json"; then
    echo "$me: self-test: --prepare did not hand the cached table over" >&2
    cat "$t/prepare" >&2
    ok=
  fi
  rm -f "$t/gh-asked"
  PATH="$t/bin:$PATH" LANDING_IT_CACHE="$t/empty" DAGQ_VERIFICATION_MATERIALS="$t/materials" \
    sh "$root/scripts/landing-it.sh" >"$t/run" 2>&1 || true
  if ! grep -qF "$me: table $t/materials/it-coverage-map.json" "$t/run" || [ -e "$t/gh-asked" ]; then
    echo "$me: self-test: a run with the materials did not read their table alone" >&2
    cat "$t/run" >&2
    ok=
  fi
  rm -rf "$t"
  [ -n "$ok" ] && echo "$me: self-test: the hand-over of the table holds"
  [ -n "$ok" ]
}

# --self-test's stages: cargo is a stub that records each run and fails the
# unit stage or the IT stage as asked. A failing unit stage ends the script
# with its status before the IT stage's run; a passing one runs the IT stage
# and ends with its status. No table (every IT) and no network.
self_test_stages() {
  t=$(mktemp -d "${TMPDIR:-/tmp}/landing-it-self-test.XXXXXX")
  mkdir -p "$t/bin" "$t/empty" "$t/materials"
  printf '#!/bin/sh\nexit 1\n' >"$t/bin/gh"
  cat >"$t/bin/cargo" <<EOF
#!/bin/sh
echo "\$*" >>"$t/runs"
case "\$*" in
  *"kind(lib)"*) exit "\${STUB_UNIT_EXIT:-0}" ;;
  *) exit "\${STUB_IT_EXIT:-0}" ;;
esac
EOF
  chmod +x "$t/bin/gh" "$t/bin/cargo"
  ok=1
  # unit exit, IT exit, the status wanted, the runs wanted, what is checked
  for c in "3 0 3 1 a failing unit stage ends before the IT stage" \
    "0 5 5 2 a passing unit stage runs the IT stage and ends with its status" \
    "0 0 0 2 both stages pass"; do
    set -- $c
    want_status=$3 want_runs=$4
    unit_exit=$1 it_exit=$2
    shift 4
    : >"$t/runs"
    status=0
    (
      unset DAGQ_LANDING_BASE DAGQ_CI_KNOWN_FAILURES DAGQ_CI_FIX_RUN
      PATH="$t/bin:$PATH" LANDING_IT_CACHE="$t/empty" DAGQ_VERIFICATION_MATERIALS="$t/materials" \
        STUB_UNIT_EXIT=$unit_exit STUB_IT_EXIT=$it_exit sh "$root/scripts/landing-it.sh"
    ) >"$t/out" 2>&1 || status=$?
    runs=$(grep -c '^nextest run ' "$t/runs" || true)
    if [ "$status" -ne "$want_status" ] || [ "$runs" -ne "$want_runs" ] ||
      ! sed -n 1p "$t/runs" | grep -qF 'kind(lib)'; then
      echo "$me: self-test: $*: exit $status (want $want_status), $runs runs (want $want_runs)" >&2
      cat "$t/out" "$t/runs" >&2
      ok=
    fi
  done
  rm -rf "$t"
  [ -n "$ok" ] && echo "$me: self-test: the unit stage runs first and stops the IT stage when it fails"
  [ -n "$ok" ]
}

if [ "${1:-}" = "--self-test" ]; then
  python3 "$py" self-test "$root/scripts/landing-it-fixtures" || exit 1
  self_test_handover
  self_test_stages
  exit $?
fi
prepare=
if [ "${1:-}" = "--prepare" ]; then
  prepare=1
  shift
  materials=${DAGQ_VERIFICATION_MATERIALS:-}
  if [ -z "$materials" ] || [ ! -d "$materials" ]; then
    echo "$me: --prepare needs DAGQ_VERIFICATION_MATERIALS, a directory" >&2
    exit 2
  fi
fi
if [ $# -gt 0 ]; then
  echo "$me: unknown argument: $1" >&2
  exit 2
fi
cd "$root"

work=$(mktemp -d "${TMPDIR:-/tmp}/landing-it.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'rm -rf "$work"; exit 130' INT TERM
cache=${LANDING_IT_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/dagq/landing-it}
workflow=it-coverage-map.yml
artifact=it-coverage-map

# The table: the cached one of the latest successful run, downloaded once.
table=-
table_error=
fetch_table() {
  mkdir -p "$cache" || return 1
  if ! command -v gh >/dev/null 2>&1; then
    table_error="gh is not on PATH"
    return 0
  fi
  if ! gh run list --workflow "$workflow" --branch main --status success --limit 1 \
      --json databaseId,headSha,createdAt \
      --jq '.[0] | select(.) | "\(.databaseId) \(.headSha) \(.createdAt)"' >"$work/run" 2>"$work/gh.err"; then
    table_error="gh run list failed: $(head -n 1 "$work/gh.err")"
    return 0
  fi
  read -r run_id head_sha created_at <"$work/run" || true
  if [ -z "${run_id:-}" ]; then
    table_error="no successful run of $workflow on main"
    return 0
  fi
  if [ ! -f "$cache/$run_id/$artifact.json" ]; then
    # Downloaded into a directory of this process, then renamed into place.
    tmp="$cache/.tmp.$$"
    rm -rf "$tmp"
    if ! gh run download "$run_id" -n "$artifact" -D "$tmp" >/dev/null 2>"$work/gh.err"; then
      table_error="gh run download $run_id failed: $(head -n 1 "$work/gh.err")"
      rm -rf "$tmp"
      return 0
    fi
    printf '{"run_id": %s, "head_sha": "%s", "created_at": "%s", "fetched_at": "%s"}\n' \
      "$run_id" "$head_sha" "$created_at" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$tmp/fetched.json"
    rm -rf "$cache/$run_id"
    if ! mv "$tmp" "$cache/$run_id"; then
      rm -rf "$tmp"
      return 0
    fi
    echo "$me: fetched the table of run $run_id ($head_sha)"
  fi
  # Keep the latest table only (the hidden .tmp.* of other processes stay).
  for dir in "$cache"/*; do
    [ -d "$dir" ] && [ "$dir" != "$cache/$run_id" ] && rm -rf "$dir"
  done
  return 0
}
if [ -n "$prepare" ] || [ -z "${DAGQ_VERIFICATION_MATERIALS:-}" ]; then
  fetch_table || table_error="the cache $cache cannot be made"
  for dir in "$cache"/*; do
    if [ -f "$dir/$artifact.json" ]; then
      table="$dir/$artifact.json"
    fi
  done
elif [ -f "$DAGQ_VERIFICATION_MATERIALS/$artifact.json" ]; then
  table="$DAGQ_VERIFICATION_MATERIALS/$artifact.json"
else
  table_error="the verification's materials have no table"
fi
if [ -n "$prepare" ]; then
  if [ "$table" = - ]; then
    echo "$me: no table to hand over (${table_error:-none cached}); every IT runs" >&2
    exit 0
  fi
  cp "$table" "$DAGQ_VERIFICATION_MATERIALS/$artifact.json" || exit 2
  echo "$me: handed over $table"
  exit 0
fi
if [ "$table" != - ]; then
  [ -n "$table_error" ] && echo "$me: $table_error; using the cached table" >&2
  table_error=
  echo "$me: table $table"
fi

# The diff, the tree of the table's commit and the files changed on main
# since that commit (which an old table widens).
full=
: >"$work/diff"
: >"$work/tree"
: >"$work/since"
since=$work/since
base=${DAGQ_LANDING_BASE:-}
if [ -z "$base" ]; then
  full="DAGQ_LANDING_BASE is not set"
elif ! git diff --name-only --no-renames "$base" HEAD >"$work/diff" 2>/dev/null; then
  full="git diff $base..HEAD failed"
fi
if [ "$table" != - ]; then
  # An unreadable table is left to the choice, which runs every IT for it.
  commit=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("commit") or "")' \
    "$table" 2>/dev/null) || commit=unreadable
  if [ "$commit" = unreadable ]; then
    table_error="$table is no JSON"
  elif [ -z "$commit" ] || ! git ls-tree -r --name-only "$commit" >"$work/tree" 2>/dev/null; then
    full=${full:-"the table's commit $commit is not in this repository"}
  elif [ -n "$base" ] &&
    ! git diff --name-only --no-renames "$commit" "$base" >"$work/since" 2>/dev/null; then
    # Only an old table needs them: it then runs every IT.
    since=-
  fi
fi

set -- --diff "$work/diff" --tree "$work/tree" --table "$table" --since "$since"
[ -n "$table_error" ] && set -- "$@" --table-error "$table_error"
[ -n "$full" ] && set -- "$@" --full "$full"

# The IT nextest lists now, only when the choice can be narrowed.
list=-
if [ "$(python3 "$py" needs-list "$@")" = yes ]; then
  list="$work/list.json"
  cargo nextest list --locked --workspace -E 'kind(test) - binary(e2e)' \
    --message-format json >"$list" || exit 2
fi

known=${DAGQ_CI_KNOWN_FAILURES:-}
[ -n "$known" ] && set -- "$@" --known "$known"
python3 "$py" select "$@" --list "$list" \
  --fix-run "${DAGQ_CI_FIX_RUN:-0}" --threads "${NEXTEST_TEST_THREADS:-6}" >"$work/filters" || exit 2
unit_filter=$(sed -n 1p "$work/filters")
it_filter=$(sed -n 2p "$work/filters")

# The landing's variables are for this script, not for the tests: the tests
# of the landing verification itself check who gets them.
unset DAGQ_LANDING_BASE DAGQ_CI_KNOWN_FAILURES DAGQ_CI_FIX_RUN
rm -rf "$work"
trap - EXIT INT TERM

# Two stages, each one cargo nextest run with .config/nextest.toml's
# fail-fast, retries and flaky-result as they are: the unit tests first, and
# the IT only when they pass, so a failing unit test ends the landing in
# seconds instead of after every IT.
echo "$me: unit stage: cargo nextest run --locked --workspace --no-tests=pass -E '$unit_filter'"
status=0
cargo nextest run --locked --workspace --no-tests=pass -E "$unit_filter" || status=$?
if [ "$status" -ne 0 ]; then
  echo "$me: the unit stage failed (exit $status); the IT stage did not run" >&2
  exit "$status"
fi
if [ -z "$it_filter" ]; then
  echo "$me: no IT chosen; the IT stage does not run"
  exit 0
fi
echo "$me: IT stage: cargo nextest run --locked --workspace --no-tests=pass -E '$it_filter'"
exec cargo nextest run --locked --workspace --no-tests=pass -E "$it_filter"
