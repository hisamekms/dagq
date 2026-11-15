#!/bin/sh
# The time gate of the unit tests in src (ADR-t4948-1): a #[test] in src that
# the diff from a base commit added or changed, and that took longer than the
# threshold in the given cargo nextest output of the lib or the bin tests
# (binaries dagq and dagq::bin/dagq), fails the check unless the allow list
# names it with a reason. The rule on which reasons may put a test on the
# list is in docs/development/testing.md, section "判断と境界のtest". The
# machinery is that of scripts/check-it-test-time.sh (the gate of dagq::it,
# ADR-t1707-1), which this script does not change.
#
# Usage: sh scripts/check-lib-test-time.sh --base REV [--threshold SECS]
#                                          [--allow FILE] [LOG...]
#        sh scripts/check-lib-test-time.sh --self-test
#   --base REV        the commit to diff from (git diff REV...HEAD -- src);
#                     required
#   --threshold SECS  the longest time a test may take (default 5)
#   --allow FILE      read the allow list from FILE alone (default: the
#                     items of .config/lib-slow-allow.toml and of every
#                     .config/lib-slow-allow.d/*.toml together, under the
#                     root of the cwd's git work tree)
#   LOG               nextest output (a file, or - for stdin); stdin when none
#   --self-test       check the script on the fixtures under
#                     scripts/check-lib-test-time-fixtures/ in a temporary git
#                     repository, and remove it
#
# A test is a fn with #[test] in a file of src at HEAD that the module tree
# reaches. It is a target when a line the diff added or changed (or the place
# of a removed line, when the lines on both sides of it lie in the test) lies
# between its #[test] and its closing brace. Its name is the full nextest
# name: the module path from the crate root, the mods around the fn and the
# fn's name, joined by ::, e.g.
# application::actor_executor::tests::a_job_that_does_not_start_gives_back_no_token_the_service_handed_it_as_held.
# The module path is found by walking the `mod NAME;` declarations from
# src/lib.rs (binary dagq) and from src/main.rs (binary dagq::bin/dagq): a
# declaration reads NAME.rs or NAME/mod.rs in the directory of the declaring
# module (the file's own directory for lib.rs, main.rs and mod.rs, else the
# directory named after the file), under the inline mods around it, and a
# #[path = "P"] on the line of the declaration or the lines above it reads P
# from the declaring file's directory (the mods of that file are read from
# its own directory, as for mod.rs). The log's tests and the allow list's
# items are matched by this full name, never by the fn's name alone; a log
# line is read only when its binary is dagq or dagq::bin/dagq (never
# dagq::it, which scripts/check-it-test-time.sh reads).
#
# Limits: a change to a helper only (a fn without #[test], a fixture, code
# outside the tests) makes no test a target, even when it makes the tests
# that use it slower. A file no `mod` declaration reaches, a #[path] whose
# string spans lines or sits in a cfg_attr, a #[path] inside an inline mod
# (read as from the inline mod's directory) and a macro that expands to tests
# are not seen; the body of a macro_rules! is skipped. Braces are counted with comments, strings, raw strings and
# char literals left out. The seconds are the log's: a CI runner, a worker's
# host and the production landing verification do not take the same time.
#
# A test's time is read from its PASS line and, for a test that passed on a
# retry, its FLKY-FL (or FLAKY) line, as scripts/slow-tests.sh does; colour
# codes are removed, and a stress run's lines ([i/N] before (j/M)) are read
# too. With several lines of one test (several logs, a stress run) its time is
# their median. Only sh, a POSIX awk and git are needed.
#
# The threshold (default 5 seconds, ADR-t4948-1): a time equal to it is not
# over it.
#
# Exit status: 0 when no target is over the threshold without an item, 1 when
# one is (each with its binary, name, seconds and file on stderr), 2 on a
# usage error, an unreadable log or allow list, a malformed allow list, no
# git work tree or a git error. A target the log does not time, or an
# allow-list item absent from HEAD, is only warned about.
#
# The allow list is, by default, the items of .config/lib-slow-allow.toml and
# of every .config/lib-slow-allow.d/*.toml together; a missing directory
# reads as empty. A new item goes in the file of the task that adds it,
# .config/lib-slow-allow.d/t<task ID>.toml, never in
# .config/lib-slow-allow.toml, so that two tasks adding items change no file
# in common (an item already in .config/lib-slow-allow.toml may be fixed or
# removed there). Each file is TOML items of the form
#   [[test]]
#   name = "<the full name>"
#   reason = "<the boundary it keeps, or the task or goal that shrinks or moves it>"
#   task = <the ID of the task that added the item>
# with all three keys. An item missing a key, with a key it does not know or
# one key twice, or two items of one name, in one file or in two, make the
# list unreadable (exit 2). An item whose full name has no #[test] in src at
# HEAD is warned about by name, without changing the exit status.
#
# When DAGQ_LOCAL_CHECKS_DIR names a directory (dagq gives it to a worker's
# turns), each run also leaves there a record of itself with the kind
# lib-test-time (ADR-t2589-1; the format is in scripts/local-check-record.sh).
# Its output then comes after it ends, stdout before stderr. Without the
# variable nothing is recorded.
set -eu

me=check-lib-test-time
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-lib-test-time.sh --base REV [--threshold SECS] [--allow FILE] [LOG...] | --self-test" >&2
  exit 2
}

# tests_at_head prints every test the module tree reaches at HEAD as
# "binary<TAB>name<TAB>path<TAB>line<TAB>changed" (changed is 0 or 1). It
# extracts src of HEAD once and walks the mod declarations in one awk, so
# that the run does not fork per file.
tests_at_head() {
  git -C "$top" rev-parse --verify --quiet "$base^{commit}" >/dev/null || {
    echo "$me: not a commit: $base" >&2
    return 2
  }
  git -C "$top" diff --no-color --no-ext-diff --src-prefix=a/ --dst-prefix=b/ --unified=0 "$base...HEAD" -- 'src' > "$work/diff" || {
    echo "$me: git diff $base...HEAD failed" >&2
    return 2
  }
  # The changed lines of each file, at HEAD: "path<TAB>line", or
  # "path<TAB>lined" for lines removed between line and line + 1.
  LC_ALL=C awk '
# a +++ line is the header only right after a --- line: an added line
# beginning with ++ is not one
/^--- / && !inhunk { minus = NR; next }
/^\+\+\+ / && NR == minus + 1 {
  file = substr($0, 5)
  if (file == "/dev/null") file = ""
  else { sub(/\t$/, "", file); sub(/^b\//, "", file) }
  if (file !~ /\.rs$/) file = ""
  next
}
/^diff / { inhunk = 0 }
/^@@ / { inhunk = 1 }
/^@@ / && file != "" {
  split($3, h, ",")
  start = substr(h[1], 2) + 0
  count = (2 in h) ? h[2] + 0 : 1
  if (count == 0) { print file "\t" start "d"; next }
  for (i = 0; i < count; i++) print file "\t" (start + i)
}' "$work/diff" > "$work/changed"
  git -C "$top" ls-tree -r --name-only HEAD -- src > "$work/files" || return 2
  mkdir "$work/tree"
  if ! git -C "$top" archive --format=tar -o "$work/src.tar" HEAD src ||
    ! (cd "$work/tree" && tar -xf "$work/src.tar"); then
    echo "$me: git archive HEAD src failed" >&2
    return 2
  fi
  LC_ALL=C awk -v sq="'" -v tree="$work/tree" -v changed="$work/changed" -v files="$work/files" '
function joinp(a, b) { return a == "" ? b : a "::" b }
function norm(p,    n, parts, i, out) {
  n = split(p, parts, "/"); out = ""
  for (i = 1; i <= n; i++) {
    if (parts[i] == "." || parts[i] == "") continue
    if (parts[i] == "..") { sub(/\/?[^\/]*$/, "", out); continue }
    out = (out == "" ? "" : out "/") parts[i]
  }
  return out
}
function dirof(p) { if (p !~ /\//) return ""; sub(/\/[^\/]*$/, "", p); return p }
function push(f, b, p, c) { nq++; qf[nq] = f; qb[nq] = b; qp[nq] = p; qc[nq] = c }
# A `mod NAME;` declaration: queue the file it reads, when HEAD has it.
function declare(name,    i, mp, sub_, cand) {
  mp = prefix; sub_ = childdir
  if (inmacro) return
  for (i = 1; i <= depth; i++) {
    if (substr(stack[i], 1, 2) != "m:") return
    mp = joinp(mp, substr(stack[i], 3)); sub_ = sub_ "/" substr(stack[i], 3)
  }
  mp = joinp(mp, name)
  if (index(buf, "#[path") > 0 && pendpath != "") {
    cand = norm((depth == 0 ? dirof(file) : sub_) "/" pendpath)
    # rustc reads the mods of a #[path] file from its own directory
    if (cand in exists) push(cand, binary, mp, dirof(cand))
  } else if ((sub_ "/" name ".rs") in exists) push(sub_ "/" name ".rs", binary, mp, sub_ "/" name)
  else if ((sub_ "/" name "/mod.rs") in exists) push(sub_ "/" name "/mod.rs", binary, mp, sub_ "/" name)
}
function modname(    name) {
  if (!match(buf, /(^|[^A-Za-z0-9_])mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*$/)) return ""
  name = substr(buf, RSTART, RLENGTH)
  sub(/^[^A-Za-z_]*mod[ \t]+/, "", name); sub(/[ \t]+$/, "", name)
  return name
}
function structural(c,    name, i, found) {
  # a ; inside the brackets of a signature ([u8; 2]) does not end it
  if (c == ";") {
    if (nest > 0) { add(c); return }
    if ((name = modname()) != "") declare(name)
    buf = ""; bufstart = 0; pendpath = ""
    return
  }
  if (c == "{") {
    # the body of a macro_rules! is a pattern, not mods and tests
    if (inmacro) stack[++depth] = "o"
    else if (match(buf, /(^|[^A-Za-z0-9_])macro_rules[ \t]*![ \t]*[A-Za-z_][A-Za-z0-9_]*[ \t]*$/)) { stack[++depth] = "x"; inmacro = depth }
    else if ((name = modname()) != "") stack[++depth] = "m:" name
    else if (index(buf, "#[test]") > 0 && match(buf, /(^|[^A-Za-z0-9_])fn[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
      name = substr(buf, RSTART, RLENGTH)
      sub(/^[^A-Za-z_]*fn[ \t]+/, "", name)
      stack[++depth] = "t:" name
      begin[depth] = bufstart
    } else stack[++depth] = "o"
    buf = ""; bufstart = 0; nest = 0; pendpath = ""
    return
  }
  # c == "}"
  if (depth > 0) {
    if (substr(stack[depth], 1, 2) == "t:") {
      name = prefix
      for (i = 1; i < depth; i++)
        if (substr(stack[i], 1, 2) == "m:") name = joinp(name, substr(stack[i], 3))
      name = joinp(name, substr(stack[depth], 3))
      found = 0
      if (file in touched)
        for (i = begin[depth]; i <= ln; i++)
          if (((file, i) in hit) || (i < ln && ((file, i) in del))) { found = 1; break }
      print binary "\t" name "\t" file "\t" begin[depth] "\t" found
    }
    if (inmacro == depth) inmacro = 0
    depth--
  }
  buf = ""; bufstart = 0; nest = 0; pendpath = ""
}
function add(s) {
  if (s == "(" || s == "[") nest++
  else if ((s == ")" || s == "]") && nest > 0) nest--
  if (bufstart == 0 && s ~ /[^ \t]/) bufstart = ln
  buf = buf s
}
# parse reads one file of the tree and prints its tests; the text between
# the characters that matter is taken whole.
function parse(path,    line, rest, c, pre, k) {
  depth = 0; buf = ""; bufstart = 0; nest = 0; pendpath = ""; state = 0; ln = 0; inmacro = 0
  while ((getline line < (tree "/" path)) > 0) {
    ln++
    # the string of a #[path] is blanked below, so keep it for the mod after it
    if (state == 0 && match(line, /#\[path[ \t]*=[ \t]*"[^"]*"/)) {
      pendpath = substr(line, RSTART, RLENGTH); sub(/^[^"]*"/, "", pendpath); sub(/"$/, "", pendpath)
    }
    rest = line
    while (rest != "") {
      if (state == 1) { # block comment, nested
        if (!match(rest, /\/\*|\*\//)) { rest = ""; break }
        if (substr(rest, RSTART, 2) == "*/") { if (--bc == 0) state = 0 } else bc++
        rest = substr(rest, RSTART + 2); continue
      }
      if (state == 2) { # string
        if (!match(rest, /[\\"]/)) { rest = ""; break }
        if (substr(rest, RSTART, 1) == "\\") { rest = substr(rest, RSTART + 2); continue }
        state = 0; rest = substr(rest, RSTART + 1); continue
      }
      if (state == 3) { # raw string
        k = index(rest, "\"" rclose)
        if (k == 0) { rest = ""; break }
        state = 0; rest = substr(rest, k + 1 + length(rclose)); continue
      }
      if (!match(rest, /[{};()\[\]"\/]/ ) && index(rest, sq) == 0) { add(rest); rest = ""; break }
      if (!match(rest, /[{};()\[\]"\/]/)) { RSTART = length(rest) + 1 }
      k = index(rest, sq)
      if (k > 0 && k < RSTART) RSTART = k
      pre = substr(rest, 1, RSTART - 1); c = substr(rest, RSTART, 1); rest = substr(rest, RSTART + 1)
      if (pre != "") add(pre)
      if (c == "/") {
        if (substr(rest, 1, 1) == "/") { rest = ""; break }
        if (substr(rest, 1, 1) == "*") { state = 1; bc = 1; rest = substr(rest, 2); continue }
        add(c); continue
      }
      if (c == "\"") {
        # r"..." and r#"..."# (and br) are raw strings
        if (match(pre, /(^|[^A-Za-z0-9_])[bc]?r#*$/)) {
          k = pre; sub(/.*r/, "", k)
          if (match(buf, /[bc]?r#*$/)) buf = substr(buf, 1, RSTART - 1)
          rclose = k; state = 3; add(" "); continue
        }
        state = 2; add(" "); continue
      }
      if (c == sq) {
        if (substr(rest, 1, 1) == "\\") {
          k = index(substr(rest, 3), sq)
          rest = (k == 0) ? "" : substr(rest, k + 3); add(" "); continue
        }
        if (substr(rest, 2, 1) == sq) { rest = substr(rest, 3); add(" "); continue }
        add(c); continue
      }
      if (c == "{" || c == "}" || c == ";") { structural(c); continue }
      add(c)
    }
    if (state == 0) add(" ")
  }
  close(tree "/" path)
}
BEGIN {
  while ((getline l < changed) > 0) {
    split(l, a, "\t"); touched[a[1]] = 1
    if (a[2] ~ /d$/) del[a[1], a[2] + 0] = 1
    else hit[a[1], a[2] + 0] = 1
  }
  while ((getline l < files) > 0) exists[l] = 1
  if ("src/lib.rs" in exists) push("src/lib.rs", "dagq", "", "src")
  if ("src/main.rs" in exists) push("src/main.rs", "dagq::bin/dagq", "", "src")
  for (q = 1; q <= nq; q++) {
    if ((qb[q], qf[q]) in seen) continue
    seen[qb[q], qf[q]] = 1
    file = qf[q]; binary = qb[q]; prefix = qp[q]; childdir = qc[q]
    parse(file)
  }
}'
}

# check reads all HEAD tests (with the diff target flag), logs and allow list
# as files, and reports.
check() {
  tfile=$1; nallow=$2; shift 2
  LC_ALL=C awk -v me="$me" -v threshold="$threshold" -v allowdesc="$allowdesc" -v nallow="$nallow" -v tfile="$tfile" '
function fail_at(where, msg) { printf "%s: %s: %s\n", me, where, msg > "/dev/stderr"; bad = 1 }
function fail_allow(msg) { fail_at(FILENAME ":" FNR, msg) }
function close_item() {
  if (!initem) return
  if (iname == "") fail_at(iwhere, "an item has no name")
  else if (ireason == "") fail_at(iwhere, "item " iname " has no reason")
  else if (itask == "") fail_at(iwhere, "item " iname " has no task")
  else if (iname in allowed) fail_at(iwhere, "item " iname " is listed twice (first at " allowed[iname] ")")
  else { allowed[iname] = iwhere; anames[++na] = iname }
  initem = 0
}
BEGIN {
  # The allow files are the nallow arguments after the targets file.
  for (i = 2; i <= nallow + 1; i++) isallow[ARGV[i]] = 1
}
FNR == 1 { close_item() }
function strval(v) {
  if (v !~ /^"([^"\\]|\\.)*"$/) return "\001"
  return substr(v, 2, length(v) - 2)
}
FILENAME in isallow {
  line = $0
  sub(/^[ \t]+/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") next
  if (line ~ /^\[\[test\]\][ \t]*(#.*)?$/) { close_item(); initem = 1; iwhere = FILENAME ":" FNR; iname = ""; ireason = ""; itask = ""; split("", seenkey); next }
  if (!initem) { fail_allow("a key outside a [[test]] item"); next }
  key = line; sub(/[ \t]*=.*/, "", key)
  val = line; sub(/^[^=]*=[ \t]*/, "", val); sub(/[ \t]+#[^"]*$/, "", val); sub(/[ \t]+$/, "", val)
  if (key in seenkey) fail_allow("key " key " is given twice in one item")
  seenkey[key] = 1
  if (key == "name") { iname = strval(val); if (iname == "\001" || iname == "") { fail_allow("name is not a string"); iname = "" } }
  else if (key == "reason") { ireason = strval(val); if (ireason == "\001") { fail_allow("reason is not a string"); ireason = "" } }
  else if (key == "task") { if (val ~ /^[0-9]+$/) itask = val; else fail_allow("task is not a number") }
  else fail_allow("unknown key " key)
  next
}
FILENAME == tfile {
  split($0, t, "\t")
  existing[t[2]] = 1
  key = t[1] " " t[2]
  if (t[5] == 1 && !(key in tpath)) { tkeys[++nt] = key; tname[key] = t[2]; tpath[key] = t[3] ":" t[4] }
  next
}
{
  line = $0
  gsub(/\033\[[0-9;]*[A-Za-z]/, "", line)
  gsub(/\r/, "", line)
  split(line, f, " ")
  if (f[1] != "PASS" && f[1] != "FLKY-FL" && f[1] != "FLAKY") next
  if (index(line, "[") == 0 || index(line, "[>") > 0) next
  i = index(line, "["); j = index(line, "s]")
  if (j <= i) next
  s = substr(line, i + 1, j - i - 1) + 0
  rest = substr(line, j + 2)
  # the stress run iteration [i/N] and the test counter (j/M)
  while (match(rest, /^[ \t]*(\[[^]]*\]|\([^)]*\))/)) rest = substr(rest, RLENGTH + 1)
  n = split(rest, w, " ")
  if (n < 2 || (w[n - 1] != "dagq" && w[n - 1] != "dagq::bin/dagq")) next
  key = w[n - 1] " " w[n]
  k = ++ns[key]
  # insertion sort of the few samples of one test
  for (m = k - 1; m >= 1 && sample[key, m] > s; m--) sample[key, m + 1] = sample[key, m]
  sample[key, m + 1] = s
}
END {
  close_item()
  if (bad) exit 2
  for (x = 1; x <= na; x++)
    if (!(anames[x] in existing))
      printf "%s: warning: allow-list item %s has no #[test] in HEAD src\n", me, anames[x] > "/dev/stderr"
  over = 0; missing = 0; ok = 0
  for (x = 1; x <= nt; x++) {
    key = tkeys[x]; name = tname[key]
    if (!(key in ns)) {
      printf "%s: warning: %s (%s) was added or changed but the log has no time for it\n", me, key, tpath[key] > "/dev/stderr"
      missing++
      continue
    }
    k = ns[key]
    med = (k % 2) ? sample[key, (k + 1) / 2] : (sample[key, k / 2] + sample[key, k / 2 + 1]) / 2
    if (med > threshold + 0 && !(name in allowed)) {
      printf "%s: %s took %.3fs, over %ss, and is not in the allow list (%s) (%s)\n", me, key, med, threshold, allowdesc, tpath[key] > "/dev/stderr"
      over++
    } else ok++
  }
  printf "%s: %d added or changed test(s) in src: %d over %ss without an item, %d within the threshold or allowed, %d not timed\n", me, nt, over, threshold, ok, missing
  exit over > 0 ? 1 : 0
}' "$tfile" "$@"
}

# self_test builds a small git history from the fixtures and checks the
# script on it, case by case.
self_test() {
  fixtures=$here/check-lib-test-time-fixtures
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  fail=0
  # The cases record nothing, even in a turn of dagq, except the record cases.
  unset DAGQ_LOCAL_CHECKS_DIR
  repo=$tmp/repo
  mkdir -p "$repo"
  g() { git -C "$repo" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  g init -q
  cp -R "$fixtures/base/." "$repo/"
  g add -A && g commit -q -m base
  cp -R "$fixtures/head/." "$repo/"
  g add -A && g commit -q -m head
  # The log: printf %b turns the \033 of the coloured line into escapes.
  printf '%b' "$(cat "$fixtures/nextest.log")" > "$tmp/nextest.log"
  cp "$fixtures/allow.toml" "$tmp/allow.toml"

  set +e
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/allow.toml" "$tmp/nextest.log") > "$tmp/out" 2> "$tmp/err"
  status=$?
  set -e
  expect() { # case, then grep -q for (present) or absent
    if grep -qF -- "$3" "$tmp/err"; then found=present; else found=absent; fi
    if [ "$2" = "$found" ]; then
      echo "$me --self-test: $1: ok"
    else
      echo "$me --self-test: $1: FAILED (want \"$3\" $2 in stderr)" >&2
      fail=1
    fi
  }
  if [ "$status" -eq 1 ]; then
    echo "$me --self-test: exit 1 with tests over the threshold: ok"
  else
    echo "$me --self-test: exit $status, want 1" >&2; fail=1
  fi
  expect added_over_threshold present "dagq gate::tests::a_new_slow_test took 7.000s, over 5s"
  expect name_seconds_and_file present "gate::tests::a_new_slow_test took 7.000s, over 5s, and is not in the allow list ($tmp/allow.toml) (src/gate.rs:"
  expect body_changed_over_threshold present "dagq gate::tests::a_test_whose_body_changed took 6.000s"
  expect allowed absent "gate::tests::an_allowed_slow_test "
  expect unchanged_slow_test absent "gate::tests::an_unchanged_slow_test"
  expect helper_change_only absent "gate::tests::a_test_whose_helper_changed"
  expect at_the_threshold_is_not_over absent "gate::tests::a_test_at_the_threshold"
  expect fast_test absent "gate::tests::a_new_fast_test"
  expect coloured_line present "dagq gate::tests::a_coloured_slow_test took 6.500s"
  expect dagq_it_line_not_read present "warning: dagq gate::tests::a_test_only_dagq_it_times ("
  expect dagq_it_line_not_over absent "a_test_only_dagq_it_times took"
  expect mod_rs_and_a_directory present "dagq application::actor::tests::an_actor_test took 8.000s"
  expect two_files_down present "dagq application::actor::inner::tests::a_test_two_files_down took 8.000s"
  expect not_by_fn_name absent "dagq actor::tests::an_actor_test"
  expect path_attribute present "dagq store::tests::a_test_under_a_path_attribute took 9.000s"
  expect inline_mod_declares_a_file present "dagq store::parts::part::a_test_in_an_inline_mods_file took 9.000s"
  expect bin_file present "dagq::bin/dagq bin_only::a_bin_test took 6.000s"
  expect bin_inline_mod present "dagq::bin/dagq daemon_tests::a_bin_inline_test took 6.000s"
  expect bin_not_read_as_lib absent ": dagq bin_only::a_bin_test"
  expect file_no_mod_reaches absent "orphan::"
  expect semicolon_in_the_signature present "dagq gate::tests::an_array_test took 9.000s"
  expect mods_of_a_path_file present "dagq store::helped::inner::a_test_under_a_path_files_mod took 9.000s"
  expect plus_plus_line_changed present "dagq gate::more::a_test_with_a_plus_plus_line took 9.000s"
  expect after_a_plus_plus_line present "dagq gate::more::a_test_after_a_plus_plus_line took 9.000s"
  expect macro_rules_body_skipped absent "a_macro_pattern_test"
  expect after_a_c_raw_string present "dagq gate::more::a_test_after_a_c_raw_string took 9.000s"
  cat "$tmp/err" >&2

  # Membership is checked even without a diff or timings. A full name
  # exists; the shorter name deliberately does not.
  cat > "$tmp/membership.toml" <<'EOF'
[[test]]
name = "application::actor::inner::tests::a_test_two_files_down"
reason = "B: the boundary of the fixture"
task = 4948

[[test]]
name = "inner::tests::a_test_two_files_down"
reason = "B: absent fixture"
task = 4948
EOF
  for want in 0 1 2; do
    set +e
    case "$want" in
      0) (cd "$repo" && sh "$script" --base HEAD --allow "$tmp/membership.toml" /dev/null) ;;
      1) (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/membership.toml" "$tmp/nextest.log") ;;
      2) (cd "$repo" && sh "$script" --base HEAD --allow "$tmp/membership.toml" "$tmp/no-such.log") ;;
    esac > "$tmp/out" 2> "$tmp/err"
    status=$?
    set -e
    if [ "$status" -eq "$want" ]; then
      echo "$me --self-test: absent_allow_item_exit_$want: ok"
    else
      echo "$me --self-test: absent_allow_item_exit_$want: exit $status, want $want" >&2
      fail=1
    fi
    if [ "$want" -ne 2 ]; then
      expect absent_allow_item_warning present "warning: allow-list item inner::tests::a_test_two_files_down has no #[test]"
      expect full_allow_item_no_warning absent "warning: allow-list item application::actor::inner::tests::a_test_two_files_down"
    fi
  done

  # An allow-list item lets its test through; the whole fixture allowed is
  # exit 0.
  cp "$tmp/allow.toml" "$tmp/all.toml"
  for n in gate::tests::a_new_slow_test gate::tests::a_test_whose_body_changed gate::tests::a_coloured_slow_test \
    application::actor::tests::an_actor_test application::actor::inner::tests::a_test_two_files_down \
    store::tests::a_test_under_a_path_attribute store::parts::part::a_test_in_an_inline_mods_file \
    bin_only::a_bin_test daemon_tests::a_bin_inline_test gate::tests::an_array_test \
    store::helped::inner::a_test_under_a_path_files_mod gate::more::a_test_with_a_plus_plus_line \
    gate::more::a_test_after_a_plus_plus_line gate::more::a_test_after_a_c_raw_string; do
    printf '\n[[test]]\nname = "%s"\nreason = "B: the boundary of the fixture"\ntask = 4948\n' "$n" >> "$tmp/all.toml"
  done
  set +e
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/all.toml" "$tmp/nextest.log") > /dev/null 2> "$tmp/err"
  all_status=$?
  # Within a higher threshold nothing is over: exit 0.
  (cd "$repo" && sh "$script" --base HEAD~1 --threshold 60 --allow "$tmp/allow.toml" "$tmp/nextest.log") > /dev/null 2>&1
  status=$?
  (cd "$repo" && sh "$script" --threshold 5 "$tmp/nextest.log") > /dev/null 2>&1
  usage_status=$?
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/allow.toml" "$tmp/no-such.log") > /dev/null 2>&1
  unreadable_status=$?
  printf '[[test]]\nname = "x"\n' > "$tmp/bad.toml"
  printf '[[test]]\nname = "x"\nname = "y"\nreason = "r"\ntask = 1\n' > "$tmp/twice.toml"
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/twice.toml" "$tmp/nextest.log") > /dev/null 2>&1
  twice_status=$?
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/bad.toml" "$tmp/nextest.log") > /dev/null 2>&1
  bad_allow_status=$?
  # The repository's own allow list, the file and the directory together,
  # reads without an error (no diff from HEAD, so no target).
  (cd "$here/.." && sh "$script" --base HEAD /dev/null) > /dev/null 2>&1
  repo_allow_status=$?
  set -e
  for c in "every_target_allowed $all_status 0" "within_the_threshold $status 0" "no_base $usage_status 2" "unreadable_log $unreadable_status 2" "item_without_reason $bad_allow_status 2" "key_given_twice $twice_status 2" "repository_allow_list $repo_allow_status 0"; do
    set -- $c
    if [ "$2" -eq "$3" ]; then echo "$me --self-test: $1: exit $2: ok"
    else echo "$me --self-test: $1: exit $2, want $3" >&2; fail=1; fi
  done

  # The default allow list: .config/lib-slow-allow.toml and the task files
  # of .config/lib-slow-allow.d together, under the work tree's root (the
  # cases run in a subdirectory). They are removed after the cases, so the
  # records below see the tree they expect.
  mkdir -p "$repo/.config"
  cp "$fixtures/allow.toml" "$repo/.config/lib-slow-allow.toml"
  default_case() { # case, want exit, then the arguments
    dc=$1; dw=$2; shift 2
    set +e
    (cd "$repo/src" && sh "$script" --base HEAD~1 "$@" "$tmp/nextest.log") > "$tmp/out" 2> "$tmp/err"
    ds=$?
    set -e
    if [ "$ds" -eq "$dw" ]; then echo "$me --self-test: $dc: exit $ds: ok"
    else echo "$me --self-test: $dc: exit $ds, want $dw" >&2; cat "$tmp/err" >&2; fail=1; fi
  }
  default_case no_task_directory_reads_as_empty 1
  expect no_task_directory_not_allowed present "dagq gate::tests::a_new_slow_test took 7.000s"
  expect no_task_directory_file_allowed absent "gate::tests::an_allowed_slow_test "
  mkdir -p "$repo/.config/lib-slow-allow.d"
  printf '[[test]]\nname = "gate::tests::a_new_slow_test"\nreason = "B: a task file item"\ntask = 1\n' > "$repo/.config/lib-slow-allow.d/t1.toml"
  default_case task_file_item 1
  expect task_file_item_allowed absent "gate::tests::a_new_slow_test took"
  expect task_file_item_file_allowed absent "gate::tests::an_allowed_slow_test "
  expect task_file_item_others_not_allowed present "dagq gate::tests::a_coloured_slow_test took 6.500s"
  printf '[[test]]\nname = "gate::tests::a_new_slow_test"\nreason = "B: the same name again"\ntask = 2\n' > "$repo/.config/lib-slow-allow.d/t2.toml"
  default_case name_in_two_task_files 2
  expect name_in_two_task_files_message present "t2.toml:1: item gate::tests::a_new_slow_test is listed twice"
  rm "$repo/.config/lib-slow-allow.d/t2.toml"
  printf '[[test]]\nname = "gate::tests::an_allowed_slow_test"\nreason = "B: in the file too"\ntask = 3\n' > "$repo/.config/lib-slow-allow.d/t3.toml"
  default_case name_in_the_file_and_a_task_file 2
  printf '[[test]]\nname = "gate::tests::a_coloured_slow_test"\n' > "$repo/.config/lib-slow-allow.d/t3.toml"
  default_case task_file_item_without_reason 2
  expect task_file_item_without_reason_message present "t3.toml:1: item gate::tests::a_coloured_slow_test has no reason"
  # --allow FILE reads that file alone, not the broken task file.
  default_case allow_reads_one_file 1 --allow "$tmp/allow.toml"
  expect allow_reads_one_file_not_the_directory present "dagq gate::tests::a_new_slow_test took 7.000s"
  rm -rf "$repo/.config"

  # The records: none without DAGQ_LOCAL_CHECKS_DIR; with it one of the
  # kind lib-test-time, its exit status, its log, the same output, and the
  # tree of the work tree with an uncommitted and an untracked change.
  records=$tmp/records
  mkdir -p "$records"
  echo '// changed' >> "$repo/src/lib.rs"
  echo new > "$repo/untracked"
  (cd "$repo" && sh "$script" --base HEAD~1 --allow "$tmp/allow.toml" "$tmp/nextest.log") > "$tmp/plain.out" 2> "$tmp/plain.err" || :
  set +e
  [ -z "$(ls -A "$records")" ]; record_case no_record_without_the_variable $?
  (cd "$repo" && DAGQ_LOCAL_CHECKS_DIR=$records sh "$script" --base HEAD~1 --allow "$tmp/allow.toml" "$tmp/nextest.log") > "$tmp/rec.out" 2> "$tmp/rec.err"
  status=$?
  [ "$status" -eq 1 ] && cmp -s "$tmp/plain.err" "$tmp/rec.err" && cmp -s "$tmp/plain.out" "$tmp/rec.out"
  record_case recorded_run_keeps_its_exit_and_output $?
  set -- "$records"/*.json
  [ $# -eq 1 ] && grep -q '"exit_status":1,' "$1" && grep -q '"kind":"lib-test-time",' "$1"
  record_case one_record_of_the_kind $?
  log=$(sed 's/.*"log":"\([^"]*\)".*/\1/' "$1")
  cat "$tmp/rec.out" "$tmp/rec.err" > "$tmp/rec.log"
  [ -f "$records/$log" ] && cmp -s "$tmp/rec.log" "$records/$log"; record_case log_beside_the_record $?
  committed=$(git -C "$repo" rev-parse 'HEAD^{tree}')
  clone=$tmp/clone
  git clone -q "$repo" "$clone"
  cp "$repo/src/lib.rs" "$clone/src/lib.rs"; cp "$repo/untracked" "$clone/"
  git -C "$clone" add -A
  want_tree=$(git -C "$clone" write-tree)
  [ "$want_tree" != "$committed" ] && grep -q "\"tree_start\":\"$want_tree\",\"tree_end\":\"$want_tree\"" "$1"
  record_case tree_with_uncommitted_and_untracked $?
  [ -z "$(git -C "$repo" diff --cached --name-only)" ]; record_case real_index_unchanged $?
  set -e

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

record_case() { # case, then the condition's result (0 is ok)
  if [ "$2" -eq 0 ]; then echo "$me --self-test: $1: ok"
  else echo "$me --self-test: $1: FAILED" >&2; fail=1; fi
}

base=""
threshold=5
allow=""

if [ "${1:-}" = "--self-test" ]; then
  [ $# -eq 1 ] || usage "--self-test takes no other argument"
  self_test
  exit 0
fi

# With the records' directory, run again as a child and record that run.
if [ -n "${DAGQ_LOCAL_CHECKS_DIR:-}" ] && [ -z "${CHECK_LIB_TEST_TIME_RECORDED:-}" ]; then
  if [ ! -r "$here/local-check-record.sh" ]; then
    echo "$me: no record of this run: $here/local-check-record.sh is missing" >&2
  elif ! record=$(mktemp -d "${TMPDIR:-/tmp}/$me-record.XXXXXX"); then
    echo "$me: no record of this run: cannot make a temporary directory" >&2
  else
    . "$here/local-check-record.sh"
    trap 'rm -rf "$record"' EXIT
    local_check_begin sh "$0" "$@" || :
    set +e
    CHECK_LIB_TEST_TIME_RECORDED=1 sh "$script" "$@" > "$record/out" 2> "$record/err"
    status=$?
    cat "$record/out" "$record/err" > "$record/log"
    local_check_end "$status" lib-test-time "$record/log" || :
    cat "$record/out"
    cat "$record/err" >&2
    exit "$status"
  fi
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage "--base needs a revision"; base=$2; shift 2 ;;
    --threshold) [ $# -ge 2 ] || usage "--threshold needs a number"; threshold=$2; shift 2 ;;
    --allow) [ $# -ge 2 ] || usage "--allow needs a file"; allow=$2; shift 2 ;;
    -h|--help) awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"; exit 0 ;;
    --) shift; break ;;
    -?*) usage "unknown argument: $1" ;;
    *) break ;;
  esac
done

[ -n "$base" ] || usage "--base is required"
case "$threshold" in
  ''|*[!0-9.]*|*.*.*|.) usage "--threshold is not a number: $threshold" ;;
esac
top=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "$me: not in a git work tree" >&2
  exit 2
}
for log in "$@"; do
  if [ "$log" != - ] && { [ ! -r "$log" ] || [ -d "$log" ]; }; then
    echo "$me: cannot read $log" >&2
    exit 2
  fi
done
if [ $# -eq 0 ]; then set -- -; fi
# The allow files go before the logs in the arguments: append them, then
# move the logs behind them.
nlogs=$#
if [ -n "$allow" ]; then
  allowdesc=$allow
  set -- "$@" "$allow"
else
  allowdesc=".config/lib-slow-allow.toml or .config/lib-slow-allow.d/t<task ID>.toml"
  set -- "$@" "$top/.config/lib-slow-allow.toml"
  for f in "$top/.config/lib-slow-allow.d"/*.toml; do
    [ -e "$f" ] || continue
    set -- "$@" "$f"
  done
fi
nallow=$(($# - nlogs))
i=0
while [ "$i" -lt "$nlogs" ]; do
  set -- "$@" "$1"
  shift
  i=$((i + 1))
done
i=0
for f in "$@"; do
  [ "$i" -lt "$nallow" ] || break
  if [ ! -r "$f" ] || [ -d "$f" ]; then
    echo "$me: cannot read $f" >&2
    exit 2
  fi
  i=$((i + 1))
done

work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
trap 'rm -rf "$work"' EXIT
set +e
tests_at_head > "$work/targets"
status=$?
set -e
[ "$status" -eq 0 ] || exit 2
set +e
check "$work/targets" "$nallow" "$@"
status=$?
set -e
exit "$status"
