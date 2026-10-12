#!/bin/sh
# Check that the lines a change adds to the documents under docs/design keep
# one sentence to a line (docs/development/documents.md, section "design",
# "形と予算" and "検査の境界"): a line on which a sentence ends with 「。」 and
# another follows fails. Only the lines the diff from the base to the work
# tree adds or changes are checked, so the lines already there are not; the
# boundaries (frontmatter, fenced code blocks, inline code, table rows,
# headings, 「。」 inside brackets and quotes) are in that section, and this
# script only applies them. The sizes and line lengths are checked by
# scripts/check-design-docs.sh.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location. Only sh, a POSIX awk and git are
# needed.
#
# Usage: sh scripts/check-design-sentences.sh [--base REF | --all]
#        sh scripts/check-design-sentences.sh --self-test
#   --base REF   check the lines the diff from REF to the work tree adds
#                (default: the merge base of HEAD with main, or with
#                origin/main when there is no main; HEAD when neither exists).
#                A file git does not track yet is added as a whole.
#   --all        check every line of every document, to see how the
#                boundaries fall on the documents as they are
#   --self-test  check the script on the fixtures under
#                scripts/check-design-sentences-fixtures/ in temporary git
#                repositories, and remove them
#
# Exit 0 when no checked line holds two sentences, 1 when one does (each
# file, line number and line go to stderr), 2 on a usage error, a base that
# is not a commit, a failing git, a document whose path holds a newline or
# that cannot be read, or no docs/design.
set -eu

me=check-design-sentences
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-design-sentences.sh [--base REF | --all] | --self-test" >&2
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
  LC_ALL=C awk -v me="$me" '
# Remove inline code: a run of n backticks opens, the next run of exactly n
# backticks on the line closes; without one the run stays as text.
function strip_code(s,    out, i, L, n, j, k, m, found) {
  out = ""; i = 1; L = length(s)
  while (i <= L) {
    if (substr(s, i, 1) != "`") { out = out substr(s, i, 1); i++; continue }
    n = 0; while (substr(s, i + n, 1) == "`") n++
    j = i + n; k = j; found = 0
    while (k <= L) {
      if (substr(s, k, 1) != "`") { k++; continue }
      m = 0; while (substr(s, k + m, 1) == "`") m++
      if (m == n) { found = 1; break }
      k += m
    }
    if (found) i = k + n
    else { out = out substr(s, i, n); i = j }
  }
  return out
}
# Whether a sentence ends with 「。」 outside brackets and quotes and some text
# other than closing brackets, emphasis marks (* and _) and blanks (the
# full-width space too) follows it on the line.
function two_sentences(s,    i, L, c, depth, rest) {
  if (!index(s, "。")) return 0
  L = length(s); depth = 0
  for (i = 1; i <= L; i++) {
    c = substr(s, i, 1)
    if (c == "(" || c == "[") { depth++; continue }
    if (c == ")" || c == "]") { if (depth > 0) depth--; continue }
    if (c < "\200") continue
    c = substr(s, i, 3)
    if (c in opener) { depth++; i += 2; continue }
    if (c in closer) { if (depth > 0) depth--; i += 2; continue }
    if (c == "。" && depth == 0) {
      rest = substr(s, i + 3)
      while (rest != "") {
        if (index(" \t\r)]*_", substr(rest, 1, 1))) { rest = substr(rest, 2); continue }
        if (substr(rest, 1, 3) in closer || substr(rest, 1, 3) == "　") { rest = substr(rest, 4); continue }
        break
      }
      if (rest != "") return 1
      i += 2
    }
  }
  return 0
}
function check_file(path,    n, l, nr, start, k, fence, t, rc) {
  n = 0
  while ((rc = (getline l < path)) > 0) lines[++n] = l
  close(path)
  if (rc < 0) { printf "%s: cannot read %s\n", me, path > "/dev/stderr"; unreadable = 1; return }
  # frontmatter: a first line of only --- up to the next line of only ---
  start = 1
  if (n > 0 && lines[1] == "---")
    for (k = 2; k <= n; k++) if (lines[k] == "---") { start = k + 1; break }
  fence = 0
  for (nr = start; nr <= n; nr++) {
    t = lines[nr]; sub(/^[ \t]+/, "", t)
    if (substr(t, 1, 3) == "```") { fence = !fence; continue }
    if (fence) continue
    if (!(all[path] || ((path, nr) in want))) continue
    if (substr(t, 1, 1) == "|" || substr(t, 1, 1) == "#") continue
    if (two_sentences(strip_code(lines[nr]))) {
      printf "%s: %s:%d: %s\n", me, path, nr, lines[nr] > "/dev/stderr"
      bad++
    }
  }
  checked++
}
BEGIN {
  split("（ 「 『 【 〔 ［ ｛ 〈 《 “ ‘", a, " "); for (k in a) opener[a[k]] = 1
  split("） 」 』 】 〕 ］ ｝ 〉 》 ” ’", a, " "); for (k in a) closer[a[k]] = 1
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
    printf "%s: %d line(s) with two sentences; write each sentence on a line of its own (docs/development/documents.md, section design, 形と予算)\n", me, bad > "/dev/stderr"
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

# self_test builds small git repositories from the fixtures and checks the
# script on them, case by case.
self_test() {
  fixtures=$here/check-design-sentences-fixtures
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  # fresh: a repository on main with old.md committed, which has a line of
  # two sentences (line 4) already.
  fresh() {
    rm -rf "$r"
    mkdir -p "$r/docs/design"
    g init -q
    g symbolic-ref HEAD refs/heads/main
    printf '# old\n\n一つ目の文。\n既にある二つの文。直さずに残す。\n' >"$r/docs/design/old.md"
    g add -A && g commit -q -m base
  }
  # check LABEL WANT [ARGS...] runs the script in the repository.
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
  # has / lacks LABEL STRING look for STRING in the last check's output.
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

  # The lines already there are not checked, the added and changed ones are.
  fresh
  check unchanged 0
  printf '足した一つの文。\n' >>"$r/$d/old.md"
  check added_one_sentence 0
  lacks old_line_not_checked "$d/old.md:4:"
  printf '足した二つの文。もう一つ。\n' >>"$r/$d/old.md"
  check added_two_sentences 1
  has added_line "$d/old.md:6: 足した二つの文。もう一つ。"
  lacks old_line_still_not_checked "$d/old.md:4:"
  fresh
  sed 's/直さずに残す/変えた/' "$r/$d/old.md" >"$r/$d/old.tmp" && mv "$r/$d/old.tmp" "$r/$d/old.md"
  check changed_line 1
  has changed_line_named "$d/old.md:4: 既にある二つの文。変えた。"
  g add -A
  check staged_change 1
  has staged_line_named "$d/old.md:4:"
  check all_lines 1 --all
  has all_names_the_old_line "$d/old.md:4:"

  # Hunks git would join by its config keep their line numbers, a renamed
  # document is not new whatever the config says, and a CRLF line ending is
  # not a second sentence.
  fresh
  g config diff.interHunkContext 5
  sed -e '1s/.*/# 変えた見出し/' -e '3s/.*/近くの二つの文。もう一つ。/' "$r/$d/old.md" >"$r/$d/old.tmp" && mv "$r/$d/old.tmp" "$r/$d/old.md"
  check joined_hunks 1
  has joined_hunks_line "$d/old.md:3:"
  fresh
  g config diff.renames false
  g mv "$r/$d/old.md" "$r/$d/moved.md"
  check renamed_not_new 0
  printf '改行がCRLFの一つの文。\r\n' >>"$r/$d/moved.md"
  check crlf 0

  # The boundaries: what is left out, then what fails, in a new file.
  fresh
  cp "$fixtures/left-out.md" "$r/$d/"
  check left_out_untracked 0
  g add -A
  check left_out_staged 0
  fresh
  mkdir -p "$r/$d/sub"
  cp "$fixtures/two-sentences.md" "$r/$d/sub/"
  check two_sentences 1
  has after_a_bracket "$d/sub/two-sentences.md:3:"
  has list_item "$d/sub/two-sentences.md:4:"
  has continuation "$d/sub/two-sentences.md:5:"
  has after_inline_code "$d/sub/two-sentences.md:6:"
  has after_the_fence "$d/sub/two-sentences.md:11:"
  has blockquote "$d/sub/two-sentences.md:12:"
  has after_a_full_width_space "$d/sub/two-sentences.md:13:"
  has unclosed_frontmatter "$d/sub/two-sentences.md:2:"
  has count "8 line(s) with two sentences"
  lacks fence_left_out "$d/sub/two-sentences.md:9:"

  # Files outside docs/design and not *.md are not checked.
  fresh
  mkdir -p "$r/docs/plans"
  printf '二つの文。もう一つ。\n' >"$r/docs/plans/p.md"
  printf '二つの文。もう一つ。\n' >"$r/$d/notes.txt"
  check outside_and_not_md 0

  # Names git quotes even with core.quotePath off (", \, a tab, a control
  # character it writes in octal) are read back, untracked and tracked.
  fresh
  q='docs/design/a"b.md' b='docs/design/c\d.md' t=$(printf 'docs/design/t\tu.md') o=$(printf 'docs/design/o\001p.md')
  for f in "$q" "$b" "$t" "$o"; do printf '一つ目の文。\n二つ目と三つ目。同じ行。\n' >"$r/$f"; done
  check quoted_untracked 1
  for f in "$q" "$b" "$t" "$o"; do has "quoted_untracked $f" "$f:2:"; done
  has quoted_untracked_count "4 line(s) with two sentences"
  g add -A && g commit -q -m quoted
  check quoted_committed 0
  for f in "$q" "$b" "$t" "$o"; do printf '足した二つの文。もう一つ。\n' >>"$r/$f"; done
  check quoted_tracked 1
  for f in "$q" "$b" "$t" "$o"; do has "quoted_tracked $f" "$f:3:"; done
  lacks quoted_tracked_old_line "$q:2:"
  fresh
  printf '一つの文。\n' >"$r/$d/new
line.md"
  check newline_in_a_path 2
  has newline_named "cannot check a path with a newline"
  check newline_in_a_path_all 2 --all
  has newline_named_all "cannot check a path with a newline"
  # A document that cannot be read stops the check: git fails on it in the
  # diff mode, the reading fails in --all (skipped when the user reads it
  # anyway, as root does).
  fresh
  printf '足した一つの文。\n' >>"$r/$d/old.md"
  chmod 000 "$r/$d/old.md"
  if [ -r "$r/$d/old.md" ]; then
    echo "$me --self-test: unreadable: skipped (the file stays readable)"
  else
    check unreadable_git_fails 2
    check unreadable_all 2 --all
    has unreadable_named "cannot read $d/old.md"
  fi
  chmod 644 "$r/$d/old.md"

  # The base: --base, the merge base with main, HEAD without main.
  fresh
  base=$(g rev-parse HEAD)
  g checkout -q -b work
  printf '枝の二つの文。もう一つ。\n' >>"$r/$d/old.md"
  g add -A && g commit -q -m 'two sentences on a branch'
  check default_base_is_the_merge_base_with_main 1
  has committed_line "$d/old.md:5:"
  check explicit_base 1 --base "$base"
  check base_at_head 0 --base HEAD
  g branch -q -D main
  check no_main_checks_since_head 0
  has no_main_message "the lines changed since HEAD are checked"
  printf '作業の二つの文。もう一つ。\n' >>"$r/$d/old.md"
  check no_main_work_tree 1
  has no_main_line "$d/old.md:6:"
  check base_not_a_commit 2 --base no-such-ref
  check unknown_argument 2 --no-such-flag
  check base_and_all 2 --all --base HEAD

  # No docs/design: nothing to check.
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
    -h|--help) sed -n '2,34p' "$0"; exit 0 ;;
    *) usage "unknown argument: $1" ;;
  esac
done
[ "$all" = 0 ] || [ -z "$base" ] || usage "--all takes no --base"
run
