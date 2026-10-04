#!/bin/sh
# The time gate of the tests in tests/it (ADR-t1707-1): a test of the one
# integration test binary (dagq::it) that the diff from a base commit added or
# changed, and that took longer than the threshold in the given cargo nextest
# output, fails the check unless the allow list names it with a reason. The
# threshold, the allow list's place and format, the naming rule and the CI
# step are in docs/design/slow-tests.md, section "itのtestの時間の関門"; this
# script only applies them.
#
# Usage: sh scripts/check-it-test-time.sh --base REV [--threshold SECS]
#                                         [--allow FILE] [LOG...]
#        sh scripts/check-it-test-time.sh --self-test
#   --base REV        the commit to diff from (git diff REV...HEAD -- tests/it);
#                     required
#   --threshold SECS  the longest time a test may take (default 5)
#   --allow FILE      the allow list (default .config/it-slow-allow.toml)
#   LOG               nextest output (a file, or - for stdin); stdin when none
#   --self-test       check the script on the fixtures under
#                     scripts/check-it-test-time-fixtures/ in a temporary git
#                     repository, and remove it
#
# A test is a fn with #[test] in a file under tests/it at HEAD. It is a target
# when a line the diff added or changed (or the place of a removed line) lies
# between its #[test] and its closing brace. Its name is the full nextest name:
# the file's path under tests/it without .rs (a #[path] in tests/it/main.rs
# wins), the mods around the fn and the fn's name, joined by ::, e.g.
# runtime_handoff::review_verdicts::reviews_that_ended_before_the_exec_are_applied_not_run_again.
# The log's tests and the allow list's items are matched by this full name,
# never by the fn's name alone.
#
# Limits: a change to a helper or a fixture only (a fn without #[test], files
# under tests/common, a stub's script) makes no test a target, even when it
# makes the tests that use it slower. Braces are counted with comments,
# strings, raw strings and char literals left out; a macro that expands to
# tests is not seen. The seconds are the log's: a CI runner and the
# production gate do not take the same time.
#
# A test's time is read from its PASS line and, for a test that passed on a
# retry, its FLKY-FL (or FLAKY) line, as scripts/slow-tests.sh does; colour
# codes are removed, and a stress run's lines ([i/N] before (j/M)) are read
# too. With several lines of one test (several logs, a stress run) its time is
# their median. Only sh, a POSIX awk and git are needed.
#
# Exit status: 0 when no target is over the threshold without an item, 1 when
# one is (each with its name, seconds and file on stderr), 2 on a usage error,
# an unreadable log or allow list, a malformed allow list or a git error. A
# target the log does not time is only warned about.
set -eu

me=check-it-test-time
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-it-test-time.sh --base REV [--threshold SECS] [--allow FILE] [LOG...] | --self-test" >&2
  exit 2
}

# targets prints one line per test the diff added or changed:
# "name<TAB>path<TAB>line", from the git repository of the current directory.
targets() {
  top=$(git rev-parse --show-toplevel) || return 2
  git -C "$top" rev-parse --verify --quiet "$base^{commit}" >/dev/null || {
    echo "$me: not a commit: $base" >&2
    return 2
  }
  diff=$(git -C "$top" diff --no-color --no-ext-diff --unified=0 "$base...HEAD" -- 'tests/it') || {
    echo "$me: git diff $base...HEAD failed" >&2
    return 2
  }
  # The changed lines of each file, at HEAD: "path<TAB>line", or
  # "path<TAB>lined" for lines removed between line and line + 1.
  changed=$(printf '%s\n' "$diff" | LC_ALL=C awk '
/^\+\+\+ / {
  file = substr($0, 5)
  if (file == "/dev/null") file = ""
  else { sub(/\t$/, "", file); sub(/^b\//, "", file) }
  if (file !~ /\.rs$/) file = ""
  next
}
/^@@ / && file != "" {
  split($3, h, ",")
  start = substr(h[1], 2) + 0
  count = (2 in h) ? h[2] + 0 : 1
  if (count == 0) { print file "\t" start "d"; next }
  for (i = 0; i < count; i++) print file "\t" (start + i)
}')
  [ -n "$changed" ] || return 0
  # The #[path] attributes of tests/it/main.rs: "path<TAB>mod".
  paths=$(git -C "$top" show "HEAD:tests/it/main.rs" 2>/dev/null | LC_ALL=C awk '
/#\[path *= *"/ {
  p = $0; sub(/.*#\[path *= *"/, "", p); sub(/".*/, "", p)
  if ($0 !~ /\][ \t]*(pub[ \t]+)?mod +[A-Za-z_][A-Za-z0-9_]* *;/) next
}
p != "" && /mod +[A-Za-z_][A-Za-z0-9_]* *;/ {
  m = $0; sub(/.*mod +/, "", m); sub(/ *;.*/, "", m)
  n = split(p, parts, "/"); out = ""
  for (i = 1; i <= n; i++) {
    if (parts[i] == "." || parts[i] == "") continue
    if (parts[i] == ".." ) { sub(/\/?[^\/]*$/, "", out); continue }
    out = (out == "" ? "" : out "/") parts[i]
  }
  print "tests/it/" out "\t" m
  p = ""
  next
}
/[^ \t]/ { p = "" }') || paths=""
  for file in $(printf '%s\n' "$changed" | cut -f1 | LC_ALL=C sort -u); do
    lines=$(printf '%s\n' "$changed" | awk -F'\t' -v f="$file" '$1 == f { printf "%s ", $2 }')
    prefix=$(printf '%s\n' "$paths" | awk -F'\t' -v f="$file" '$1 == f { print $2; exit }')
    if [ -z "$prefix" ]; then
      prefix=${file#tests/it/}
      prefix=${prefix%.rs}
      prefix=${prefix%/mod}
      [ "$prefix" = main ] && prefix=""
      prefix=$(printf '%s' "$prefix" | sed 's|/|::|g')
    fi
    git -C "$top" show "HEAD:$file" | LC_ALL=C awk -v sq="'" -v file="$file" -v prefix="$prefix" -v lines="$lines" '
function structural(c,    name, i, m, found) {
  # a ; inside the brackets of a signature ([u8; 2]) does not end it
  if (c == ";") { if (nest == 0) { buf = ""; bufstart = 0 } else add(c); return }
  if (c == "{") {
    if (match(buf, /(^|[^A-Za-z0-9_])mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*$/)) {
      name = substr(buf, RSTART, RLENGTH)
      sub(/^[^A-Za-z_]*mod[ \t]+/, "", name); sub(/[ \t]+$/, "", name)
      stack[++depth] = "m:" name
    } else if (index(buf, "#[test]") > 0 && match(buf, /(^|[^A-Za-z0-9_])fn[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
      name = substr(buf, RSTART, RLENGTH)
      sub(/^[^A-Za-z_]*fn[ \t]+/, "", name)
      stack[++depth] = "t:" name
      begin[depth] = bufstart
    } else {
      stack[++depth] = "o"
    }
    buf = ""; bufstart = 0; nest = 0
    return
  }
  # c == "}"
  if (depth > 0) {
    if (substr(stack[depth], 1, 2) == "t:") {
      name = prefix
      for (i = 1; i < depth; i++)
        if (substr(stack[i], 1, 2) == "m:")
          name = (name == "" ? "" : name "::") substr(stack[i], 3)
      name = (name == "" ? "" : name "::") substr(stack[depth], 3)
      found = 0
      for (i = begin[depth]; i <= NR; i++)
        if ((i in hit) || (i < NR && (i in del))) { found = 1; break }
      if (found) print name "\t" file "\t" begin[depth]
    }
    depth--
  }
  buf = ""; bufstart = 0; nest = 0
}
function add(c) {
  if (c == "(" || c == "[") nest++
  else if ((c == ")" || c == "]") && nest > 0) nest--
  if (bufstart == 0 && c !~ /[ \t]/) bufstart = NR
  buf = buf c
}
BEGIN {
  n = split(lines, l, " ")
  for (i = 1; i <= n; i++) {
    if (l[i] ~ /d$/) del[l[i] + 0] = 1
    else hit[l[i] + 0] = 1
  }
  state = 0  # 0 code, 1 block comment, 2 string, 3 raw string
}
{
  line = $0; len = length(line); i = 1
  while (i <= len) {
    c = substr(line, i, 1)
    if (state == 1) {
      if (substr(line, i, 2) == "*/") { if (--bc == 0) state = 0; i += 2; continue }
      if (substr(line, i, 2) == "/*") { bc++; i += 2; continue }
      i++; continue
    }
    if (state == 2) {
      if (c == "\\") { i += 2; continue }
      if (c == "\"") state = 0
      i++; continue
    }
    if (state == 3) {
      if (c == "\"" && substr(line, i + 1, hashes) == rclose) { state = 0; i += 1 + hashes; continue }
      i++; continue
    }
    if (substr(line, i, 2) == "//") break
    if (substr(line, i, 2) == "/*") { state = 1; bc = 1; i += 2; continue }
    if (c == "\"") { state = 2; add(" "); i++; continue }
    if ((c == "r" || substr(line, i, 2) == "br") && (i == 1 || substr(line, i - 1, 1) !~ /[A-Za-z0-9_]/)) {
      j = (c == "r") ? i + 1 : i + 2
      if (c == "b" && substr(line, i + 1, 1) != "r") { add(c); i++; continue }
      k = j; while (substr(line, k, 1) == "#") k++
      if (substr(line, k, 1) == "\"") {
        hashes = k - j; rclose = substr(line, j, hashes)
        state = 3; add(" "); i = k + 1; continue
      }
    }
    if (c == sq) {
      if (substr(line, i + 1, 1) == "\\") {
        k = i + 3; while (k <= len && substr(line, k, 1) != sq) k++
        add(" "); i = k + 1; continue
      }
      if (substr(line, i + 2, 1) == sq) { add(" "); i += 3; continue }
      add(c); i++; continue
    }
    if (c == "{" || c == "}" || c == ";") { structural(c); i++; continue }
    add(c); i++
  }
  if (state == 0) add(" ")
}'
  done
}

# check reads the targets on stdin and the logs and the allow list as files,
# and reports.
check() {
  tfile=$1; shift
  LC_ALL=C awk -v me="$me" -v threshold="$threshold" -v allowfile="$allow" -v tfile="$tfile" '
function fail_allow(msg) { printf "%s: %s:%d: %s\n", me, allowfile, FNR, msg > "/dev/stderr"; bad = 1 }
function close_item() {
  if (!initem) return
  if (iname == "") fail_allow("an item has no name")
  else if (ireason == "") fail_allow("item " iname " has no reason")
  else if (itask == "") fail_allow("item " iname " has no task")
  else if (iname in allowed) fail_allow("item " iname " is listed twice")
  else allowed[iname] = 1
  initem = 0
}
function strval(v) {
  if (v !~ /^"([^"\\]|\\.)*"$/) return "\001"
  return substr(v, 2, length(v) - 2)
}
FILENAME == allowfile {
  line = $0
  sub(/^[ \t]+/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") next
  if (line ~ /^\[\[test\]\][ \t]*(#.*)?$/) { close_item(); initem = 1; iname = ""; ireason = ""; itask = ""; split("", seenkey); next }
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
  if (!allowdone) { close_item(); allowdone = 1 }
  split($0, t, "\t")
  if (!(t[1] in tpath)) { tnames[++nt] = t[1]; tpath[t[1]] = t[2] ":" t[3] }
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
  if (n < 2 || w[n - 1] != "dagq::it") next
  name = w[n]
  k = ++ns[name]
  # insertion sort of the few samples of one test
  for (m = k - 1; m >= 1 && sample[name, m] > s; m--) sample[name, m + 1] = sample[name, m]
  sample[name, m + 1] = s
}
END {
  if (!allowdone) close_item()
  if (bad) exit 2
  over = 0; missing = 0; ok = 0
  for (x = 1; x <= nt; x++) {
    name = tnames[x]
    if (!(name in ns)) {
      printf "%s: warning: %s (%s) was added or changed but the log has no time for it\n", me, name, tpath[name] > "/dev/stderr"
      missing++
      continue
    }
    k = ns[name]
    med = (k % 2) ? sample[name, (k + 1) / 2] : (sample[name, k / 2] + sample[name, k / 2 + 1]) / 2
    if (med > threshold + 0 && !(name in allowed)) {
      printf "%s: dagq::it %s took %.3fs, over %ss, and is not in %s (%s)\n", me, name, med, threshold, allowfile, tpath[name] > "/dev/stderr"
      over++
    } else ok++
  }
  printf "%s: %d added or changed test(s) in tests/it: %d over %ss without an item, %d within the threshold or allowed, %d not timed\n", me, nt, over, threshold, ok, missing
  exit over > 0 ? 1 : 0
}' "$allow" "$tfile" "$@"
}

# self_test builds a small git history from the fixtures and checks the
# script on it, case by case.
self_test() {
  fixtures=$here/check-it-test-time-fixtures
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  fail=0
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
  expect over_threshold present "dagq::it gate_fixture::a_new_slow_test took 7.000s"
  expect allowed absent "gate_fixture::an_allowed_slow_test "
  expect unchanged_slow_test absent "gate_fixture::an_unchanged_slow_test"
  expect not_in_the_log present "warning: gate_fixture::a_test_the_log_does_not_time ("
  expect not_in_the_log_is_not_over absent "dagq::it gate_fixture::a_test_the_log_does_not_time took"
  expect coloured_line present "dagq::it gate_fixture::a_coloured_slow_test took 6.500s"
  expect nested_mod_full_name present "dagq::it nested_fixture::review_verdicts::inner::a_nested_test_whose_body_changed took 8.000s"
  expect nested_mod_not_by_fn_name absent "nested_fixture::a_nested_test_whose_body_changed"
  expect path_attribute present "dagq::it renamed_by_path::a_test_under_a_path_attribute took 9.000s"
  expect fast_test absent "gate_fixture::a_new_fast_test"
  expect one_line_path_attribute present "dagq::it one_liner::a_test_after_a_one_line_path took 9.000s"
  expect semicolon_in_the_signature present "dagq::it gate_fixture::an_array_test took 9.000s"
  cat "$tmp/err" >&2

  # Within a higher threshold nothing is over: exit 0.
  set +e
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
  # The repository's own allow list reads without an error (no diff from
  # HEAD, so no target).
  (cd "$here/.." && sh "$script" --base HEAD --allow "$here/../.config/it-slow-allow.toml" /dev/null) > /dev/null
  repo_allow_status=$?
  set -e
  for c in "within_the_threshold $status 0" "no_base $usage_status 2" "unreadable_log $unreadable_status 2" "item_without_reason $bad_allow_status 2" "key_given_twice $twice_status 2" "repository_allow_list $repo_allow_status 0"; do
    set -- $c
    if [ "$2" -eq "$3" ]; then echo "$me --self-test: $1: exit $2: ok"
    else echo "$me --self-test: $1: exit $2, want $3" >&2; fail=1; fi
  done

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

base=""
threshold=5
allow=.config/it-slow-allow.toml

if [ "${1:-}" = "--self-test" ]; then
  [ $# -eq 1 ] || usage "--self-test takes no other argument"
  self_test
  exit 0
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage "--base needs a revision"; base=$2; shift 2 ;;
    --threshold) [ $# -ge 2 ] || usage "--threshold needs a number"; threshold=$2; shift 2 ;;
    --allow) [ $# -ge 2 ] || usage "--allow needs a file"; allow=$2; shift 2 ;;
    -h|--help) sed -n '2,47p' "$0"; exit 0 ;;
    --) shift; break ;;
    -?*) usage "unknown argument: $1" ;;
    *) break ;;
  esac
done

[ -n "$base" ] || usage "--base is required"
case "$threshold" in
  ''|*[!0-9.]*|*.*.*|.) usage "--threshold is not a number: $threshold" ;;
esac
if [ ! -r "$allow" ] || [ -d "$allow" ]; then
  echo "$me: cannot read $allow" >&2
  exit 2
fi
for log in "$@"; do
  if [ "$log" != - ] && { [ ! -r "$log" ] || [ -d "$log" ]; }; then
    echo "$me: cannot read $log" >&2
    exit 2
  fi
done

work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
trap 'rm -rf "$work"' EXIT
set +e
targets > "$work/targets"
status=$?
set -e
[ "$status" -eq 0 ] || exit 2
if [ $# -eq 0 ]; then set -- -; fi
set +e
check "$work/targets" "$@"
status=$?
set -e
exit "$status"
