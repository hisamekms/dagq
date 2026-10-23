#!/bin/sh
# Check the lines a change adds to the tests for the forms of two rules of
# docs/development/testing.md that a line alone shows, so the program review
# of a run returns them before the test-rules agent starts. Only the lines
# the diff from the base to the work tree adds or changes are checked, so the
# lines already there are not. The rules themselves and their exceptions are
# in testing.md; the forms a line cannot show (an unbounded join, Barrier or
# output(), an IT that lays out the cases of a decision) are left to the
# test-rules agent (.dagq/agents/test-rules/AGENT.md).
#
# A-262 ("判断と境界のtest"): a line added to a #[cfg(test)] module of
# src/**/*.rs that holds SystemTime::now( ,
# thread::sleep( or Command::new( . A line is in a #[cfg(test)] module when
#   - it lies from a line of only #[cfg(test)] to the next line of only "}"
#     (a // comment may follow it) at the indent of the "mod NAME {" that
#     follows it (other attribute lines, doc comments and blank lines may
#     come between, a #[path attribute may not); braces in strings are not
#     told apart, so a line of a string that is such a "}" ends the module
#     early and the lines after it are not checked, or
#   - its file is a module a line of only #[cfg(test)] declares at the top
#     level ("mod NAME;" at indent 0) of its parent module's file
#     (src/lib.rs or src/main.rs for src/NAME.rs, DIR.rs or DIR/mod.rs for
#     DIR/NAME.rs or DIR/NAME/mod.rs), or a module below such a file.
#   Any other #[cfg(...)] (cfg(all(test, ...)), a #[cfg(test)] on a fn or an
#   impl, a nested "mod NAME;") is not told apart and not checked. Left out,
#   as a line cannot tell it from what the rule allows: Instant::now( , which
#   the unit tests read for an origin they pass on as a value (an Instant has
#   no other constructor).
# A-137 ("待ちの上限"): a line added to tests/** that writes its own wait for
# a file (while [ ! -f , while [ ! -e , until [ -f , until [ -e , with a
# sleep command after it before the next "; done" on the line; each loop of
# the line is looked at) instead of await_file or await_path of
# tests/common. A sleep command is the word sleep at the start of a command
# (after the start of the line, ; & | ( ) { } or the word do, then or else,
# and followed by a blank, ; ) & | or the end of the line); a "sleep" in a
# path or another word (sleep.stop, "$dir/sleep", echo sleep) is not one. A
# loop without a sleep command on its line (a spin, or a body on lines of its
# own) is not checked, nor a line that defines the
# shell function await_file itself ("await_file() {"), nor the strings of
# src/ (a unit test there runs no shell; its strings are inputs).
# Comment lines (// after blanks) are not checked for either rule, nor for
# A-137 a shell comment (# at the start of the line or after a blank, to the
# end of the line).
#
# `test-rules: allow A-<number> <reason>` on the line, or on the line before
# it when that line holds only the mark in a comment (//, ///, //!, /* or #
# before it), takes that one line out of the rule with that number; a mark
# after the code of a line takes out only that line, not the next. A mark without a
# number or a reason on a checked line fails (the end of a block comment
# after the number is no reason); whether the reason holds is left to the
# test-rules agent.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location. Only sh, a POSIX awk and git are
# needed.
#
# Usage: sh scripts/check-test-rules.sh [--base REF]
#        sh scripts/check-test-rules.sh --self-test
#   --base REF   check the lines the diff from REF to the work tree adds
#                (default: the merge base of HEAD with main, or with
#                origin/main when there is no main; HEAD when neither exists).
#                A file git does not track yet is added as a whole.
#   --self-test  check the script on the fixtures under
#                scripts/check-test-rules-fixtures/ in temporary git
#                repositories, and remove them
#
# Exit 0 when no checked line breaks a rule, 1 when one does (each file, line
# number, rule and line go to stderr), 2 on a usage error, a base that is not
# a commit, a failing git, a path that holds a newline or a file that cannot
# be read, or neither src nor tests.
set -eu

me=check-test-rules
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-test-rules.sh [--base REF] | --self-test" >&2
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
# target prints "<line>\t<path>" for a file under tests or a .rs under src,
# and stops with exit 2 on a path with a newline, which the targets cannot
# carry.
function target(path, nr) {
  if (path !~ /^tests\// && path !~ /^src\/.*\.rs$/) return
  if (index(path, "\n")) { printf "%s: cannot check a path with a newline: %s\n", me, path > "/dev/stderr"; exit 2 }
  print nr "\t" path
}'

# added <base> prints "<line>\t<path>" for each line under src and tests that
# the diff from the base to the work tree adds, and "0\t<path>" for a file git
# does not track (every line). Renames are found whatever the config says, so
# a moved file is not checked as new. Exit 2 when git fails.
added() {
  git -c core.quotePath=false diff -U0 --inter-hunk-context=0 --find-renames \
    --no-color --no-ext-diff --no-textconv --src-prefix=a/ --dst-prefix=b/ \
    "$1" -- src tests >"$work/diff" || exit 2
  git -c core.quotePath=false ls-files --others --exclude-standard -- src tests >"$work/untracked" || exit 2
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
function indent_of(s) { match(s, /^[ \t]*/); return substr(s, 1, RLENGTH) }
function trim(s) { sub(/^[ \t]+/, "", s); sub(/[ \t\r]+$/, "", s); return s }
# Whether a trimmed line may stand between #[cfg(test)] and its mod.
function between(t) { return t == "" || (t ~ /^#\[/ && t !~ /^#\[path/) || t ~ /^\/\/\// }
function read_file(path, arr,    n, l, rc) {
  n = 0
  while ((rc = (getline l < path)) > 0) arr[++n] = l
  close(path)
  if (rc < 0) return -1
  return n
}
# declares(parent, name) is 2 when the file parent declares "mod name;" at
# the top level under a #[cfg(test)], 1 when it declares it without, 0 when
# not at all (or the file cannot be read). Each parent is read once.
function declares(parent, name,    n, k, j, t, m, pl) {
  if (!(parent in parsed)) {
    parsed[parent] = 1
    n = read_file(parent, pl)
    for (k = 1; k <= n; k++) {
      if (pl[k] !~ /^(pub(\([^)]*\))? )?mod [A-Za-z0-9_]+;/) continue
      m = pl[k]; sub(/^(pub(\([^)]*\))? )?mod /, "", m); sub(/;.*/, "", m)
      if (decl[parent, m] < 1) decl[parent, m] = 1
      for (j = k - 1; j >= 1; j--) {
        t = trim(pl[j])
        if (t == "#[cfg(test)]" && indent_of(pl[j]) == "") { decl[parent, m] = 2; break }
        if (!between(t)) break
      }
    }
  }
  return decl[parent, name] + 0
}
# test_file(path) is 1 when the whole file is a module under #[cfg(test)].
function test_file(path,    m, name, dir, d, p1, p2, r) {
  if (path in is_test) return is_test[path]
  is_test[path] = 0
  m = path; sub(/\.rs$/, "", m); sub(/\/mod$/, "", m)
  if (m !~ /\//) return 0
  name = m; sub(/.*\//, "", name)
  dir = m; sub(/\/[^\/]*$/, "", dir)
  if (dir == "src") {
    if (name == "lib" || name == "main") return 0
    p1 = "src/lib.rs"; p2 = "src/main.rs"
  } else { p1 = dir ".rs"; p2 = dir "/mod.rs" }
  r = 0
  d = declares(p1, name); if (d == 2) r = 1; else if (d == 1 && test_file(p1)) r = 1
  if (!r) { d = declares(p2, name); if (d == 2) r = 1; else if (d == 1 && test_file(p2)) r = 1 }
  is_test[path] = r
  return r
}
# Whether a loop that waits for a file starts on the line and sleeps before
# the next "; done" after it; each loop on the line is looked at. A shell
# comment (a # at the start or after a blank) and what follows it are not.
function file_wait(t,    rest, seg, at) {
  sub(/(^|[ \t])#.*$/, "", t)
  rest = t
  while (1) {
    at = 0
    if (match(rest, /while[ \t]+\[[ \t]+![ \t]+-[fe][ \t]/)) at = RSTART
    if (match(rest, /until[ \t]+\[[ \t]+-[fe][ \t]/) && (!at || RSTART < at)) at = RSTART
    if (!at) return 0
    rest = substr(rest, at); seg = rest
    if (match(seg, /;[ \t]*done/)) seg = substr(seg, 1, RSTART)
    if (seg ~ /(^|[;&|(){}])[ \t]*sleep([ \t;)&|]|$)/ || seg ~ /(^|[ \t;])(do|then|else)[ \t]+sleep([ \t;)&|]|$)/) return 1
    rest = substr(rest, 6)
  }
}
# mark(s) is the number ("A-137") of the mark on s when it has a reason, ""
# when s has no mark, and "-" when its mark has no number or no reason. The
# end of a block comment (*/) and what follows it are no reason.
function mark(s,    k, code, rest) {
  k = index(s, "test-rules: allow")
  if (!k) return ""
  rest = substr(s, k + 17)
  if (!match(rest, /^[ \t]+A-[0-9]+/)) return "-"
  code = substr(rest, RSTART, RLENGTH); sub(/^[ \t]+/, "", code)
  rest = substr(rest, RLENGTH + 1)
  if (rest !~ /^[ \t]/) return "-"
  if (index(rest, "*/")) rest = substr(rest, 1, index(rest, "*/") - 1)
  gsub(/[ \t\r]/, "", rest)
  return rest == "" ? "-" : code
}
# mark_only(s) is 1 when s holds nothing before its mark but the opening of
# a comment, so the mark is for the line after it.
function mark_only(s,    k, head) {
  k = index(s, "test-rules: allow")
  if (!k) return 0
  head = substr(s, 1, k - 1); gsub(/[ \t]/, "", head)
  return head == "//" || head == "///" || head == "//!" || head == "/*" || head == "#"
}
# A valid mark for the rule on the line, or on the line before it when that
# line holds only the mark.
function allowed(code, nr) {
  return mark(lines[nr]) == code || (nr > 1 && mark_only(lines[nr - 1]) && mark(lines[nr - 1]) == code)
}
function report(path, nr, code, what) {
  printf "%s: %s:%d: %s: %s: %s\n", me, path, nr, code, what, lines[nr] > "/dev/stderr"
  bad++
}
function check_file(path,    n, nr, t, src, whole, intest, pending, pind, endl, l) {
  delete lines
  n = read_file(path, lines)
  if (n < 0) { printf "%s: cannot read %s\n", me, path > "/dev/stderr"; unreadable = 1; return }
  src = path ~ /^src\//
  whole = src && test_file(path)
  intest = 0; pending = 0
  for (nr = 1; nr <= n; nr++) {
    l = lines[nr]; t = trim(l)
    if (src && !whole) {
      if (intest) { sub(/\r$/, "", l); sub(/[ \t]*\/\/.*$/, "", l); if (l == endl) intest = 0 }
      else if (pending) {
        if (t ~ /^(pub(\([^)]*\))? )?mod [A-Za-z0-9_]+ \{$/ && indent_of(l) == pind) { intest = 1; endl = pind "}"; pending = 0 }
        else if (!between(t)) pending = 0
      }
      if (!intest && !pending && t == "#[cfg(test)]") { pending = 1; pind = indent_of(l) }
    }
    if (!(all[path] || ((path, nr) in want))) continue
    if (mark(lines[nr]) == "-")
      report(path, nr, "mark", "test-rules: allow needs A-<number> and a reason")
    if (substr(t, 1, 2) == "//") continue
    if (src && (whole || intest)) {
      if ((index(t, "SystemTime::now(") || index(t, "thread::sleep(") || index(t, "Command::new(")) && !allowed("A-262", nr))
        report(path, nr, "A-262", "a unit test uses the real clock, a sleep or an external process (testing.md, 判断と境界のtest)")
    }
    if (!src && !index(t, "await_file() {") && file_wait(t) && !allowed("A-137", nr))
      report(path, nr, "A-137", "a wait for a file of its own; use await_file or await_path of tests/common (testing.md, 待ちの上限)")
  }
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
    printf "%s: %d line(s) break a rule of the tests; fix them or mark one with test-rules: allow A-<number> <reason> (docs/development/testing.md)\n", me, bad > "/dev/stderr"
    exit 1
  }
  printf "%s: %d file(s) with lines to check under src and tests: ok\n", me, checked + 0
}' "$1"
}

run() {
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
  cd "$root"
  if [ ! -d src ] && [ ! -d tests ]; then
    echo "$me: neither src nor tests found under $root" >&2
    exit 2
  fi
  work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$work"' EXIT
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
  set +e
  check "$work/targets"
  status=$?
  set -e
  exit "$status"
}

# self_test builds small git repositories from the fixtures and checks the
# script on them, case by case.
self_test() {
  fixtures=$here/check-test-rules-fixtures
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  # fresh: a repository on main with the fixtures' base files committed;
  # src/old.rs already breaks A-262 in its test module (line 8) and
  # tests/it/old.rs A-137 (line 1).
  fresh() {
    rm -rf "$r"
    mkdir -p "$r"
    cp -R "$fixtures/base/." "$r/"
    g init -q
    g symbolic-ref HEAD refs/heads/main
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
  # put FIXTURE PATH copies a fixture file into the repository.
  put() {
    mkdir -p "$(dirname "$r/$2")"
    cp "$fixtures/$1" "$r/$2"
  }

  # The lines already there are not checked, the added and changed ones are.
  fresh
  check unchanged 0
  check base_at_head 0 --base HEAD
  awk '$0 == "}" { print "    #[test]"; print "    fn added() {}" } { print }' "$r/src/old.rs" >"$r/src/old.tmp" && mv "$r/src/old.tmp" "$r/src/old.rs"
  check added_clean_line 0
  lacks old_lines_not_checked "src/old.rs:8:"
  sed 's/let a = 1;/let a = std::time::SystemTime::now();/' "$r/src/old.rs" >"$r/src/old.tmp" && mv "$r/src/old.tmp" "$r/src/old.rs"
  check changed_line 1
  has changed_line_named "src/old.rs:3: A-262"
  lacks old_line_still_not_checked "src/old.rs:8:"
  g add -A
  check staged_change 1

  # The detections, untracked and then staged: each form in an inline test
  # module, in a whole-file test module and below it, and the waits in tests.
  fresh
  put detected/inline.rs src/inline.rs
  put detected/lib.rs src/lib.rs
  put detected/tests.rs src/whole/tests.rs
  put detected/whole.rs src/whole.rs
  put detected/deeper.rs src/whole/tests/deeper.rs
  put detected/wait.rs tests/it/wait.rs
  put detected/wait.sh tests/fixtures/wait.sh
  put detected/adapter.rs src/infrastructure/adapter.rs
  check detected_untracked 1
  lacks inline_instant_left_out "src/inline.rs:6:"
  has inline_system_time "src/inline.rs:7: A-262"
  has inline_sleep "src/inline.rs:8: A-262"
  has inline_command "src/inline.rs:9: A-262"
  lacks src_wait_left_out "src/inline.rs:10:"
  lacks outside_the_module "src/inline.rs:19:"
  has nested_module "src/inline.rs:17: A-262"
  has whole_file "src/whole/tests.rs:3: A-262"
  has below_whole_file "src/whole/tests/deeper.rs:2: A-262"
  has wait_while_f "tests/it/wait.rs:2: A-137"
  has wait_while_e "tests/it/wait.rs:3: A-137"
  has wait_until_f "tests/it/wait.rs:4: A-137"
  has wait_until_e "tests/it/wait.rs:5: A-137"
  has wait_in_a_shell_file "tests/fixtures/wait.sh:1: A-137"
  has mark_without_reason "tests/it/wait.rs:7: mark"
  has mark_with_another_number "tests/it/wait.rs:9: A-137"
  has done_in_the_file_name "tests/it/wait.rs:10: A-137"
  has second_loop_of_a_line "tests/it/wait.rs:11: A-137"
  has block_comment_mark_without_reason "tests/it/wait.rs:12: mark"
  has not_taken_out_by_it "tests/it/wait.rs:12: A-137"
  has block_comment_mark_on_its_own_line "tests/it/wait.rs:13: mark"
  has not_taken_out_by_the_line_before "tests/it/wait.rs:14: A-137"
  has infrastructure_command "src/infrastructure/adapter.rs:3: A-262"
  has infrastructure_sleep "src/infrastructure/adapter.rs:4: A-262"
  lacks trailing_mark_takes_out_its_line "src/infrastructure/adapter.rs:5:"
  has trailing_mark_not_for_the_next_line "src/infrastructure/adapter.rs:6: A-262"
  lacks trailing_wait_mark_takes_out_its_line "tests/it/wait.rs:15:"
  has trailing_wait_mark_not_for_the_next_line "tests/it/wait.rs:16: A-137"
  has sleep_after_and "tests/it/wait.rs:17: A-137"
  has count "24 line(s) break a rule"
  g add -A
  check detected_staged 1
  has count_staged "24 line(s) break a rule"

  # What is not detected: outside #[cfg(test)], the forms not told apart,
  # await_file and its definition, a loop without a sleep on its line,
  # comments (shell comments too), the marks with a reason.
  fresh
  put left-out/outside.rs src/outside.rs
  put left-out/main.rs src/main.rs
  put left-out/declared.rs src/declared.rs
  put left-out/marked.rs tests/it/marked.rs
  put left-out/comment.sh tests/fixtures/comment.sh
  check left_out_untracked 0
  g add -A
  check left_out_staged 0

  # Files outside src and tests, and not .rs under src, are not checked.
  fresh
  mkdir -p "$r/scripts"
  cp "$fixtures/detected/wait.sh" "$r/scripts/wait.sh"
  cp "$fixtures/detected/wait.sh" "$r/src/wait.sh"
  check outside_src_and_tests 0

  # A renamed file is not new whatever the config says.
  fresh
  g config diff.renames false
  g mv "$r/src/old.rs" "$r/src/moved.rs"
  check renamed_not_new 0

  # A path with a newline stops the check.
  fresh
  printf 'while [ ! -f go ]; do sleep 1; done\n' >"$r/tests/new
line.sh"
  check newline_in_a_path 2
  has newline_named "cannot check a path with a newline"

  # The base: --base, the merge base with main, HEAD without main.
  fresh
  base=$(g rev-parse HEAD)
  g checkout -q -b work
  put detected/wait.rs tests/it/wait.rs
  g add -A && g commit -q -m 'waits on a branch'
  check default_base_is_the_merge_base_with_main 1
  has committed_line "tests/it/wait.rs:2: A-137"
  check explicit_base 1 --base "$base"
  check base_at_branch_head 0 --base HEAD
  g branch -q -D main
  check no_main_checks_since_head 0
  has no_main_message "the lines changed since HEAD are checked"
  check base_not_a_commit 2 --base no-such-ref
  check unknown_argument 2 --no-such-flag
  check base_without_a_revision 2 --base

  # Neither src nor tests: nothing to check.
  rm -rf "$r/src" "$r/tests"
  check no_src_or_tests 2

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

base=""
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
    -h|--help) sed -n "2,$(awk '/^set -eu$/ { print NR - 1; exit }' "$0")p" "$0"; exit 0 ;;
    *) usage "unknown argument: $1" ;;
  esac
done
run
