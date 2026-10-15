#!/bin/sh
# The tests of this repository's landing verification (ADR-t1925-1, goal 157
# stage 3b): every unit test and the integration tests (IT) the diff reaches,
# in one cargo nextest run. dagq.toml's [landing_verification] runs this in
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
#                (a table, a tree, a nextest list and cases.json), and the
#                hand-over of --prepare's table to a run in the container.
#                Needs only sh and python3.
#
# Which IT (scripts/landing-it.py, choose()): over the files of
# git diff $DAGQ_LANDING_BASE..HEAD, the union of
#   - the tests the table maps each file to,
#   - the tests a changed test file defines (tests/it/<m>.rs, tests/plugin.rs,
#     crates/<c>/tests/<t>.rs),
#   - the tests that read a changed file from the repository (plugins/**,
#     dagq.toml and the like, READ_BY_TESTS),
#   - the IT nextest lists now that the table does not have (added or renamed
#     after the table was built).
# Every IT runs instead, with the reasons printed, when the narrowed IT's
# estimate is over the threshold, a common file or a src or crates .rs file
# not in the tree of the table's commit is touched, or the table cannot be
# had or is too old. The threshold, the common files and the age limit are
# written once, at the top of scripts/landing-it.py, with their grounds in
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
# Exit status: cargo nextest's; 2 when the run cannot be prepared.
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

if [ "${1:-}" = "--self-test" ]; then
  python3 "$py" self-test "$root/scripts/landing-it-fixtures" || exit 1
  self_test_handover
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

# The diff and the tree of the table's commit.
full=
: >"$work/diff"
: >"$work/tree"
base=${DAGQ_LANDING_BASE:-}
if [ -z "$base" ]; then
  full="DAGQ_LANDING_BASE is not set"
elif ! git diff --name-only --no-renames "$base" HEAD >"$work/diff" 2>/dev/null; then
  full="git diff $base..HEAD failed"
fi
if [ "$table" != - ]; then
  commit=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("commit") or "")' "$table")
  if [ -z "$commit" ] || ! git ls-tree -r --name-only "$commit" >"$work/tree" 2>/dev/null; then
    full=${full:-"the table's commit $commit is not in this repository"}
  fi
fi

set -- --diff "$work/diff" --tree "$work/tree" --table "$table"
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
filter=$(python3 "$py" select "$@" --list "$list" \
  --fix-run "${DAGQ_CI_FIX_RUN:-0}" --threads "${NEXTEST_TEST_THREADS:-6}") || exit 2

echo "$me: cargo nextest run --locked --workspace --no-tests=pass -E '$filter'"
# The landing's variables are for this script, not for the tests: the tests
# of the landing verification itself check who gets them.
unset DAGQ_LANDING_BASE DAGQ_CI_KNOWN_FAILURES DAGQ_CI_FIX_RUN
# exec skips the EXIT trap: remove the work directory first.
rm -rf "$work"
trap - EXIT INT TERM
exec cargo nextest run --locked --workspace --no-tests=pass -E "$filter"
