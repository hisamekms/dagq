#!/bin/sh
# Check that the lines a change adds to the documents under docs/design do not
# carry the detail the map layer leaves to the doc comments
# (docs/development/documents.md, section "design", "4層", "書くもの・書かな
# いもの" and "検査の境界"): (A) a line with LINE_CODE_MIN or more inline code
# spans (a row of identifiers), and (B) a run of LIST_RUN_MIN or more added
# list items (- or 1.) in a row, each with inline code on its line or its
# continuation lines (an enumeration by case or by identifier). Only the lines
# the diff from the base to the work tree adds or changes are checked, so the
# lines already there are not; the boundaries are those of
# scripts/check-design-sentences.sh and are written in that section. What it
# lets through is left to the review agent that judges the layers.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location. Only sh, a POSIX awk and git are
# needed.
#
# Usage: sh scripts/check-design-identifiers.sh [--base REF | --all]
#        sh scripts/check-design-identifiers.sh --self-test
#   --base REF   check the lines the diff from REF to the work tree adds
#                (default: the merge base of HEAD with main, or with
#                origin/main when there is no main; HEAD when neither exists).
#                A file git does not track yet is added as a whole.
#   --all        check every line of every document, to list what the checks
#                find in the documents as they are (not used by CI or review)
#   --self-test  check the script on small temporary git repositories, and
#                remove them
#
# Exit 0 when no checked line breaks (A) or (B), 1 when one does (each file,
# line number, what it broke and the line go to stderr), 2 on a usage error, a
# base that is not a commit, a failing git, a document whose path holds a
# newline or that cannot be read, or no docs/design.

# The thresholds. They lean to no false finding, as a failure sends the run
# back to the worker and counts as one of the run's automatic revisions; what
# they miss is left to the review agent that judges the layers. Measured on
# main at 8d0c08992969bcbcbe13e6df1b720c33cf811ab9 (2026-11-02):
# - Lines that add only map lines (entries to the code, promises), in 14
#   commits on main: e41e775a3, 9934d9666, 2b88666af, 8d4cd0bc4, 00fe28661,
#   5d26e2e60, 96db17bd0, 64891313f, 56f760cfb, e6c22194f, e3fe20660,
#   46c6b9c78, 299b981d7, ca3a91a0b. Their added lines have at most 5 inline
#   code spans (an entry line of ca3a91a0b) and no run of more than 1 item, so
#   with these thresholds they give 0 findings. Lines of 6 and 7 spans in the
#   documents still hold entry lines (one names three entries with their
#   modules), so (A) starts at 8.
# - The design diffs of tasks 3900 (8d0c08992), 3885 (d982e61ec) and 3886
#   (ed6de702c) add 5, 1 and 1 continuation lines of docs/design/measurement.md
#   with 0 to 1 inline code span each and no list item, and give 0 findings: a
#   threshold that caught them would catch most map lines. In that document
#   the checks find the case-by-case lists of 「区間とタグ」 (runs of 9 and 7
#   items, lines 74-94 and 109-124) and its lines of 8 and 9 spans (82, 98).
# - Lists of map entries run to 4 to 6 items (the kinds of 「台帳の形」 are 5,
#   the added sections of 81a327fc0 and 1152ba1b3 are 4), so (B) starts at 7.
# - --all on main: 570 findings on 562 lines in 41 documents (57 by (A), 513
#   lines in runs by (B), 8 lines by both); 9 of the last 80 commits that
#   change docs/design fail.
LINE_CODE_MIN=8
LIST_RUN_MIN=7
set -eu

me=check-design-identifiers
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-design-identifiers.sh [--base REF | --all] | --self-test" >&2
  exit 2
}

# unquote is an awk function that turns a path git quotes (a name with ", \
# or a control character is quoted with C escapes even with core.quotePath
# off) back into its bytes; a path git does not quote is returned as it is.
unquote='
function unquote(s,    out, i, c, L) {
  if (substr(s, 1, 1) != "\"" || substr(s, length(s), 1) != "\"") return s
  if (!("101" in oct)) for (i = 1; i < 256; i++) oct[sprintf("%03o", i)] = sprintf("%c", i)
  s = substr(s, 2, length(s) - 2); out = ""; L = length(s)
  for (i = 1; i <= L; i++) {
    c = substr(s, i, 1)
    if (c != "\\") { out = out c; continue }
    c = substr(s, ++i, 1)
    if (c ~ /[0-7]/) { out = out oct[substr(s, i, 3)]; i += 2 }
    else if (c == "n") out = out "\n"
    else if (c == "t") out = out "\t"
    else if (c == "r") out = out "\r"
    else if (c == "a") out = out "\007"
    else if (c == "b") out = out "\b"
    else if (c == "f") out = out "\f"
    else if (c == "v") out = out "\v"
    else out = out c
  }
  return out
}
# target prints "<line>\t<path>" for a document under docs/design, and stops
# with exit 2 on a path with a newline, which the targets cannot carry.
function target(path, nr) {
  if (path !~ /^docs\/design\/.*\.md$/) return
  if (index(path, "\n")) { printf "%s: cannot check a path with a newline: %s\n", me, path > "/dev/stderr"; exit 2 }
  print nr "\t" path
}'

# added <base> prints "<line>\t<path>" for each line under docs/design/**/*.md
# that the diff from the base to the work tree adds, and "0\t<path>" for a
# file git does not track (every line). Renames are found whatever the
# config says, so a moved document is not checked as new. Exit 2 when git
# fails.
added() {
  git -c core.quotePath=false diff -U0 --inter-hunk-context=0 --find-renames \
    --no-color --no-ext-diff --no-textconv --src-prefix=a/ --dst-prefix=b/ \
    "$1" -- docs/design >"$work/diff" || exit 2
  git -c core.quotePath=false ls-files --others --exclude-standard -- docs/design >"$work/untracked" || exit 2
  LC_ALL=C awk -v me="$me" "$unquote"'
/^diff --git / { head = 1; path = ""; next }
head && /^\+\+\+ / {
  path = substr($0, 5); sub(/\t$/, "", path)
  if (path == "/dev/null") path = ""; else path = substr(unquote(path), 3)
  next
}
/^@@ / {
  head = 0
  s = $0; sub(/^@@ -[0-9,]+ \+/, "", s); sub(/[^0-9].*/, "", s); nr = s + 0
  next
}
head { next }
/^\+/ { if (path != "") target(path, nr); nr++; next }
/^ / { nr++ }' "$work/diff"
  LC_ALL=C awk -v me="$me" "$unquote"'{ target(unquote($0), 0) }' "$work/untracked"
}


# check <targets>: each line of targets is "<line>\t<path>" (0 for every line
# of the file); the path is the rest of the line, so it may hold a tab.
check() {
  LC_ALL=C awk -v me="$me" -v line_min="$LINE_CODE_MIN" -v run_min="$LIST_RUN_MIN" '
# The number of inline code spans on a line: a run of n backticks opens, the
# next run of exactly n backticks on the line closes; without one the run
# stays as text and is not counted.
function count_code(s,    c, i, L, n, j, k, m, found) {
  c = 0; i = 1; L = length(s)
  while (i <= L) {
    if (substr(s, i, 1) != "`") { i++; continue }
    n = 0; while (substr(s, i + n, 1) == "`") n++
    j = i + n; k = j; found = 0
    while (k <= L) {
      if (substr(s, k, 1) != "`") { k++; continue }
      m = 0; while (substr(s, k + m, 1) == "`") m++
      if (m == n) { found = 1; break }
      k += m
    }
    if (found) { c++; i = k + n } else i = j
  }
  return c
}
function report(path, nr, what) {
  printf "%s: %s:%d: %s: %s\n", me, path, nr, what, lines[nr] > "/dev/stderr"
  bad++
}
# end_item closes the list item open now: an added item with inline code
# (on its line or its continuation lines) joins the run, any other item ends
# the run.
function end_item() {
  if (!item) return
  if (item_added && item_code) run[++nrun] = item
  else end_run()
  item = 0
}
function end_run(    k) {
  if (nrun >= run_min)
    for (k = 1; k <= nrun; k++) report(cur, run[k], "list of " nrun " items with inline code")
  nrun = 0
}
function end_block() { end_item(); end_run() }
function check_file(path,    n, l, nr, start, k, fence, t, rc, c, want_line) {
  n = 0
  split("", lines)
  while ((rc = (getline l < path)) > 0) lines[++n] = l
  close(path)
  if (rc < 0) { printf "%s: cannot read %s\n", me, path > "/dev/stderr"; unreadable = 1; return }
  cur = path; item = 0; nrun = 0
  # frontmatter: a first line of only --- up to the next line of only ---
  start = 1
  if (n > 0 && lines[1] == "---")
    for (k = 2; k <= n; k++) if (lines[k] == "---") { start = k + 1; break }
  fence = 0
  for (nr = start; nr <= n; nr++) {
    t = lines[nr]; sub(/^[ \t]+/, "", t); sub(/\r$/, "", t)
    if (substr(t, 1, 3) == "```") { end_block(); fence = !fence; continue }
    if (fence) continue
    # Table rows, headings and blank lines are not checked and end a list.
    if (t == "" || substr(t, 1, 1) == "|" || substr(t, 1, 1) == "#") { end_block(); continue }
    want_line = all[path] || ((path, nr) in want)
    c = count_code(lines[nr])
    if (want_line && c >= line_min) report(path, nr, c " inline code")
    if (t ~ /^(-|[0-9]+\.)([ \t]|$)/) {
      end_item()
      item = nr; item_added = want_line; item_code = c > 0
    } else if (item && lines[nr] ~ /^[ \t]/) {
      if (c > 0) item_code = 1
    } else end_block()
  }
  end_block()
  checked++
}
{
  k = index($0, "\t"); nr = substr($0, 1, k - 1) + 0; path = substr($0, k + 1)
  if (!(path in seen)) { seen[path] = 1; paths[++np] = path }
  if (nr == 0) all[path] = 1; else want[path, nr] = 1
}
END {
  for (i = 1; i <= np; i++) check_file(paths[i])
  if (unreadable) exit 2
  if (bad) {
    printf "%s: %d violation(s): move the identifiers and the case-by-case lists to the doc comments by their definitions and keep the entry to the code (docs/development/documents.md, section design, 4層 and 形と予算)\n", me, bad > "/dev/stderr"
    exit 1
  }
  printf "%s: %d document(s) with lines to check under docs/design: ok\n", me, checked
}' "$1"
}

run() {
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
  cd "$root"
  if [ ! -d docs/design ]; then
    echo "$me: docs/design not found under $root" >&2
    exit 2
  fi
  work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$work"' EXIT
  if [ "$all" = 1 ]; then
    # find prints one path to a line, so a path with a newline cannot be
    # told from two; stop on it as the diff mode does.
    nl='
'
    if [ -n "$(find docs/design -type f -name '*.md' -name "*$nl*")" ]; then
      echo "$me: cannot check a path with a newline under docs/design" >&2
      exit 2
    fi
    find docs/design -type f -name '*.md' | LC_ALL=C sort | awk '{ print "0\t" $0 }' >"$work/targets"
  else
    if [ -n "$base" ]; then
      git rev-parse --verify --quiet "$base^{commit}" >/dev/null || {
        echo "$me: not a commit: $base" >&2
        exit 2
      }
    else
      for ref in main origin/main; do
        if git rev-parse --verify --quiet "$ref^{commit}" >/dev/null 2>&1; then
          base=$(git merge-base "$ref" HEAD 2>/dev/null) || base=""
          break
        fi
      done
      if [ -z "$base" ]; then
        echo "$me: no merge base of HEAD with main or origin/main: the lines changed since HEAD are checked"
        base=HEAD
      fi
    fi
    added "$base" >"$work/targets"
  fi
  set +e
  check "$work/targets"
  status=$?
  set -e
  exit "$status"
}

# self_test builds small git repositories and checks the script on them, case
# by case. The lines are made from the thresholds, so the cases follow them.
self_test() {
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  # codes N prints a line of N inline code spans.
  codes() { awk -v n="$1" 'BEGIN { s = "識別子"; for (i = 1; i <= n; i++) s = s "`id" i "`・"; print s "の行。" }'; }
  # items N [PREFIX] prints N list items, each with one inline code span.
  items() { awk -v n="$1" -v p="${2:-- }" 'BEGIN { for (i = 1; i <= n; i++) print p "場面" i "は`step" i "`を書く。" }'; }
  over=$LINE_CODE_MIN under=$((LINE_CODE_MIN - 1)) run=$LIST_RUN_MIN short=$((LIST_RUN_MIN - 1))
  # fresh: a repository on main with old.md committed, which has a line of
  # many inline code spans (line 3) and a long list (lines 5 on) already.
  fresh() {
    rm -rf "$r"
    mkdir -p "$r/docs/design"
    g init -q
    g symbolic-ref HEAD refs/heads/main
    { printf '# old\n\n'; codes "$over"; printf '\n'; items "$run"; } >"$r/docs/design/old.md"
    g add -A && g commit -q -m base
  }
  check() {
    label=$1 want=$2
    shift 2
    set +e
    (cd "$r" && sh "$script" "$@") </dev/null >"$st/out" 2>"$st/err"
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
  lacks() {
    if grep -qF -- "$2" "$st/err" "$st/out"; then
      echo "$me --self-test: $1: FAILED (\"$2\" not wanted in the output)" >&2
      fail=1
    fi
  }
  d=docs/design
  n0=$((4 + run)) # the last line of old.md

  # (A): the lines already there are not checked, the added ones are.
  fresh
  check unchanged 0
  { printf '\n'; codes "$under"; } >>"$r/$d/old.md"
  check line_under_the_threshold 0
  lacks old_line_not_checked "$d/old.md:3:"
  codes "$over" >>"$r/$d/old.md"
  check line_at_the_threshold 1
  has added_line "$d/old.md:$((n0 + 3)): $over inline code: "
  lacks old_line_still_not_checked "$d/old.md:3:"
  g add -A
  check staged_line 1
  check all_lines 1 --all
  has all_names_the_old_line "$d/old.md:3:"
  has all_names_the_old_list "$d/old.md:5: list of $run items"

  # (B): a run of added items with inline code.
  fresh
  { printf '\n'; items "$short"; } >>"$r/$d/old.md"
  check list_under_the_threshold 0
  { printf '\n'; items "$run" '1. '; } >>"$r/$d/old.md"
  check list_at_the_threshold 1
  has list_named "$d/old.md:$((n0 + short + 3)): list of $run items with inline code: 1. 場面1"
  has list_last_item "$d/old.md:$((n0 + short + run + 2)): list of $run items"
  lacks old_list_not_checked "$d/old.md:5:"
  # One item appended to the list already there is not a run of added items.
  fresh
  items 1 >>"$r/$d/old.md"
  check item_appended_to_an_old_list 0
  # An item without inline code, a blank line or a paragraph breaks the run;
  # inline code on a continuation line counts and nested items join it.
  fresh
  { printf '\n'; items "$short"; printf -- '- 識別子の無い箇条。\n'; items 1; } >>"$r/$d/old.md"
  check item_without_code_breaks 0
  fresh
  { printf '\n'; items "$short"; printf '\n'; items 1; } >>"$r/$d/old.md"
  check blank_line_breaks 0
  fresh
  { printf '\n'; items "$short"; printf '段落の行。\n'; items 1; } >>"$r/$d/old.md"
  check paragraph_breaks 0
  fresh
  { printf '\n'; items "$short"; printf -- '- 続きの行に識別子がある箇条。\n  続きは`next`を書く。\n'; } >>"$r/$d/old.md"
  check code_on_a_continuation_line 1
  fresh
  { printf '\n'; items "$short"; items 1 '  - '; } >>"$r/$d/old.md"
  check nested_item_joins 1

  # What is left out: frontmatter, fenced code blocks, table rows, headings.
  fresh
  {
    printf -- '---\n'; codes "$over"; printf -- '---\n\n'
    printf '```text\n'; codes "$over"; items "$run"; printf '```\n\n'
    printf '| a | b |\n| --- | --- |\n'; codes "$over" | sed 's/^/| /; s/$/ |/'
    printf '\n'; codes "$over" | sed 's/^/## /'
    printf '\n| 表 |\n'; items "$short"; printf '| 表 |\n'; items 1
  } >"$r/$d/left-out.md"
  check left_out_untracked 0
  g add -A
  check left_out_staged 0
  fresh
  mkdir -p "$r/$d/sub"
  { printf '# new\n\n'; codes "$over"; } >"$r/$d/sub/new.md"
  check untracked_file_is_added 1
  has untracked_named "$d/sub/new.md:3:"

  # Files outside docs/design and not *.md are not checked.
  fresh
  mkdir -p "$r/docs/plans"
  codes "$over" >"$r/docs/plans/p.md"
  codes "$over" >"$r/$d/notes.txt"
  check outside_and_not_md 0

  # Names git quotes, a renamed document, a path with a newline.
  fresh
  q='docs/design/a"b.md'
  codes "$over" >"$r/$q"
  check quoted_untracked 1
  has quoted_named "$q:1:"
  fresh
  g config diff.renames false
  g mv "$r/$d/old.md" "$r/$d/moved.md"
  check renamed_not_new 0
  printf 'x\n' >"$r/$d/new
line.md"
  check newline_in_a_path 2
  check newline_in_a_path_all 2 --all

  # The base: --base, the merge base with main, HEAD without main.
  fresh
  base=$(g rev-parse HEAD)
  g checkout -q -b work
  codes "$over" >>"$r/$d/old.md"
  g add -A && g commit -q -m 'identifiers on a branch'
  check default_base_is_the_merge_base_with_main 1
  check explicit_base 1 --base "$base"
  check base_at_head 0 --base HEAD
  g branch -q -D main
  check no_main_checks_since_head 0
  has no_main_message "the lines changed since HEAD are checked"
  check base_not_a_commit 2 --base no-such-ref
  check unknown_argument 2 --no-such-flag
  check base_and_all 2 --all --base HEAD

  rm -rf "$r/docs"
  check no_docs_design 2

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

base=""
all=0
case "${1:-}" in
  --self-test)
    [ $# -eq 1 ] || usage "--self-test takes no other argument"
    self_test
    exit 0
    ;;
esac

while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage "--base needs a revision"; base=$2; shift 2 ;;
    --all) all=1; shift ;;
    -h|--help) sed -n '2,35p' "$0"; exit 0 ;;
    *) usage "unknown argument: $1" ;;
  esac
done
[ "$all" = 0 ] || [ -z "$base" ] || usage "--all takes no --base"
run
