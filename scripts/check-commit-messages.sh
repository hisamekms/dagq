#!/bin/sh
# Check the prefix of the subject of each commit of the run branch: the
# commits from the merge base of HEAD with main to HEAD. A subject passes when
# it is a type of allowed_types in .config/committed.toml (in any case, as
# committed compares them), an optional (scope)
# and !, then ": " and a subject that is not empty. fixup!, squash! and amend!
# before it are stripped first and merge commits are skipped, as the commit
# lint (committed) does with that file, which is the one place of the types.
# The commit-msg hook (.githooks/commit-msg) is the early notice and this
# script is the gate: it needs only sh, a POSIX awk and git, not committed, so
# the program review runs it on any host.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location.
#
# Usage: sh scripts/check-commit-messages.sh
#        sh scripts/check-commit-messages.sh --self-test
#   --self-test  check the script on the subjects under
#                scripts/check-commit-messages-fixtures/ (pass.txt and
#                fail.txt, one subject to a line) in temporary git
#                repositories, and remove them
#
# The base is the merge base of HEAD with main, or with origin/main when there
# is no main; with neither, nothing is checked.
#
# Exit 0 when the range is empty or every subject passes, 1 when one does not
# (each short sha and subject, and the types allowed, go to stderr), 2 on a
# usage error, no git work tree, a failing git, or no allowed_types in
# .config/committed.toml.
set -eu

me=check-commit-messages
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-commit-messages.sh [--self-test]" >&2
  exit 2
}

# types <config> prints the types of allowed_types, one to a line. The array
# may span lines; a # comment after a value is left out.
types() {
  awk '
    !on && /^[ \t]*allowed_types[ \t]*=/ { on = 1; sub(/^[^=]*=/, "") }
    on {
      line = $0
      while (match(line, /"[^"]*"|#/)) {
        tok = substr(line, RSTART, RLENGTH)
        if (tok == "#") break
        print substr(tok, 2, length(tok) - 2)
        line = substr(line, RSTART + RLENGTH)
      }
      if (index($0, "]")) exit
    }' "$1"
}

# check <types-file> reads "<sha>\t<subject>" lines and prints to stderr each
# one whose subject does not pass. Exit 1 when one does not.
check() {
  awk -F '\t' -v me="$me" '
    FNR == NR { allowed[$0] = 1; list = list (list == "" ? "" : ", ") $0; next }
    {
      sha = $1; s = substr($0, length(sha) + 2); n++
      while (match(s, /^(fixup|squash|amend)! /)) s = substr(s, RLENGTH + 1)
      ok = 0
      if (match(s, /^[^ (!:]+(\([^()]+\))?!?: /)) {
        head = substr(s, 1, RLENGTH); rest = substr(s, RLENGTH + 1)
        type = tolower(head); sub(/[(!:].*$/, "", type)
        ok = (type in allowed) && rest ~ /[^ \t]/
      }
      if (!ok) { printf "%s: %s %s\n", me, sha, substr($0, length(sha) + 2) > "/dev/stderr"; bad++ }
    }
    END {
      if (bad) {
        printf "%s: %d of %d commit(s) without an allowed prefix; start the subject with <type>[(scope)][!]: , where type is one of: %s (.config/committed.toml), and reword the commits named (the run branch is squashed when it lands)\n", me, bad, n, list > "/dev/stderr"
        exit 1
      }
      printf "%s: %d commit(s) checked: ok\n", me, n
    }' "$1" -
}

run() {
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
  cd "$root"
  git rev-parse --verify --quiet HEAD >/dev/null 2>&1 || {
    echo "$me: no git work tree with a commit under $root" >&2
    exit 2
  }
  config=.config/committed.toml
  work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$work"' EXIT
  [ -f "$config" ] && types "$config" >"$work/types" && [ -s "$work/types" ] || {
    echo "$me: no allowed_types in $config under $root" >&2
    exit 2
  }
  base=""
  for ref in main origin/main; do
    if git rev-parse --verify --quiet "$ref^{commit}" >/dev/null 2>&1; then
      base=$(git merge-base "$ref" HEAD) || {
        echo "$me: no merge base of HEAD with $ref" >&2
        exit 2
      }
      break
    fi
  done
  if [ -z "$base" ]; then
    echo "$me: neither main nor origin/main: no commit to check"
    exit 0
  fi
  git log --no-merges --format='%h%x09%s' "$base..HEAD" >"$work/commits" || {
    echo "$me: git log $base..HEAD failed" >&2
    exit 2
  }
  set +e
  check "$work/types" <"$work/commits"
  status=$?
  set -e
  exit "$status"
}

# self_test makes a repository on main with the repository's
# .config/committed.toml, and checks each fixture subject as one commit on a
# branch.
self_test() {
  fixtures=$here/check-commit-messages-fixtures
  repo_root=$(cd "$here/.." && pwd)
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  # fresh: a repository on main with the config committed, at the branch work.
  fresh() {
    rm -rf "$r"
    mkdir -p "$r/.config"
    g init -q
    g symbolic-ref HEAD refs/heads/main
    cp "$repo_root/.config/committed.toml" "$r/.config/"
    g add -A && g commit -q -m 'runtime: a base commit is not checked'
    g checkout -q -b work
  }
  commit() { g commit -q --allow-empty --cleanup=verbatim -m "$1"; }
  # check LABEL WANT runs the script in the repository.
  check_case() {
    label=$1 want=$2
    set +e
    (cd "$r" && sh "$script") </dev/null >"$st/out" 2>"$st/err"
    got=$?
    set -e
    if [ "$got" -eq "$want" ]; then
      echo "$me --self-test: $label: exit $got: ok"
    else
      echo "$me --self-test: $label: exit $got, want $want" >&2
      sed 's/^/  /' "$st/err" "$st/out" >&2
      fail=1
    fi
  }
  has() {
    if ! grep -qF -- "$2" "$st/err" "$st/out"; then
      echo "$me --self-test: $1: FAILED (want \"$2\" in the output)" >&2
      sed 's/^/  /' "$st/err" >&2
      fail=1
    fi
  }

  # The types are the seven of the config, in its order.
  got_types=$(types "$repo_root/.config/committed.toml" | tr '\n' ' ')
  if [ "$got_types" = "feat fix docs test refactor perf chore " ]; then
    echo "$me --self-test: allowed_types: ok"
  else
    echo "$me --self-test: allowed_types: got \"$got_types\"" >&2
    fail=1
  fi

  fresh
  check_case empty_range 0
  has empty_range_count "0 commit(s) checked"

  while IFS= read -r s; do
    fresh
    commit "$s"
    check_case "pass: $s" 0
  done <"$fixtures/pass.txt"

  while IFS= read -r s; do
    fresh
    commit "$s"
    check_case "fail: $s" 1
    has "fail names: $s" "$(g log -1 --format=%h)"
    has "fail lists the types: $s" "feat, fix, docs, test, refactor, perf, chore"
  done <"$fixtures/fail.txt"

  # One bad commit among good ones fails, and only it is named.
  fresh
  commit 'feat: good'
  commit 'runtime: bad'
  commit 'fix: good'
  check_case one_bad_of_three 1
  has one_bad_count "1 of 3 commit(s)"

  # A merge commit is skipped; the commits it brings from main are not in the
  # range.
  fresh
  commit 'feat: on the branch'
  g checkout -q main
  commit 'config: on main after the branch'
  g checkout -q work
  g merge -q --no-ff --no-edit main
  check_case merge_skipped 0
  has merge_count "1 commit(s) checked"

  # origin/main when there is no main; nothing to check with neither.
  fresh
  commit 'runtime: bad'
  g update-ref refs/remotes/origin/main main
  g branch -q -D main
  check_case origin_main 1
  g update-ref -d refs/remotes/origin/main
  check_case no_main 0
  has no_main_message "neither main nor origin/main"

  # No allowed_types, and a usage error.
  fresh
  printf 'style = "conventional"\n' >"$r/.config/committed.toml"
  check_case no_allowed_types 2
  rm "$r/.config/committed.toml"
  check_case no_config 2
  set +e
  sh "$script" --no-such-flag >/dev/null 2>&1
  got=$?
  set -e
  if [ "$got" -eq 2 ]; then echo "$me --self-test: unknown_argument: exit 2: ok"; else
    echo "$me --self-test: unknown_argument: exit $got, want 2" >&2
    fail=1
  fi

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

case "${1:-}" in
  --self-test)
    [ $# -eq 1 ] || usage "--self-test takes no other argument"
    self_test
    exit 0
    ;;
  -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
  "") [ $# -eq 0 ] || usage "unknown argument: $1" ;;
  *) usage "unknown argument: $1" ;;
esac
run
