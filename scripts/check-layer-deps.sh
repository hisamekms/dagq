#!/bin/sh
# Check the forbidden references between the port modules of the contexts.
# The rule (its ID C8), what
# counts in short and the allow list's place are in
# docs/design/architecture.md, sections "レイヤーの規則", "コンテキストの規則"
# and "検査の範囲"; the allow list's format is in its own header
# (.config/layer-deps-allow.txt); the details of what counts are here.
#
# C8 counts in the context modules of src/application/ports/ (every file
# but mod.rs), outside their inline modules (where `super` is the file's own
# module), inside tests too: a `super::<name>` whose name is not a module
# the file may name ($c8_ok; `super::super`, the application, is always
# allowed, but not `super::super::ports`), a glob `super::*`, and any
# `crate::application::ports` path. So a port is named by
# its module (`super::shared::Clock`), never through the re-exports of
# mod.rs, and only from the modules the rule lets the file use.
#
# L1, L2's part clippy cannot hold, L3, L6, L8 and L9 are checked by the
# Rust test of the module references (src/context_boundaries/modules.rs),
# and C3 by the visibility of the supervisor's context modules, not here.
#
# What counts is the path of a reference (`std::process::id`,
# `super::shared::Clock`), with a grouped `use crate::{a, b}` expanded
# into one path each. Comments (`//`, `//!`, `///` and `/* */`, nested ones
# too), doc links and string and character literals do not count; the
# paths in an attribute other than a cfg do.
#
# Every occurrence the rules forbid must have its (rule, path, reference) in
# the allow list (.config/layer-deps-allow.txt, or LAYER_DEPS_ALLOW_FILE),
# and every item of the allow list must still match an occurrence, so the
# task that fixes a violation also removes its item.
#
# The tree checked, and the allow list read, are those of the git work tree of
# the cwd (`git rev-parse --show-toplevel`), so a copy of the script run
# elsewhere with the cwd in a worktree checks that worktree and its allow list
# (the program review of a run; docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location. Run it from the repository root
# (`sh scripts/check-layer-deps.sh`).
# LAYER_DEPS_ROOT names another tree to check (its src/ is read) and
# LAYER_DEPS_ALLOW_FILE another allow list; --self-test checks the script
# itself on small fixtures in a temporary directory under ${TMPDIR:-target/}
# (no violation, one not in the list, a stale item, an item without a task,
# references only in comments and strings, nested block comments, a path in
# an attribute and the C8 references) and removes them.
#
# Exit 0 when every occurrence is allowed and no item is stale, 1 when an
# occurrence is not allowed, an item is stale or the allow list is malformed
# (each goes to stderr), 2 when src/ is not found.
set -eu

me=check-layer-deps
# Rule C8: each port module of src/application/ports/ and the modules it may
# name, "module:allowed,allowed"; a port module not listed may name none.
c8_ok="planning:shared execution:shared host:shared,execution observation:shared,planning,execution,host shared:planning,execution,observation,host"
script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
repo=$(git rev-parse --show-toplevel 2>/dev/null) || repo=$(cd "$(dirname "$0")/.." && pwd)

# scan prints one line per occurrence: "rule<TAB>path<TAB>reference<TAB>line",
# with paths relative to the tree's root.
scan() {
  files=$(find src -type f -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || return 0
  # shellcheck disable=SC2086
  LC_ALL=C awk -v c8_ok="$c8_ok" '
BEGIN {
  n = split(c8_ok, o, " ")
  for (i = 1; i <= n; i++) { split(o[i], kv, ":"); c8[kv[1]] = "," kv[2] "," }
}
function reset_file() {
  in_block = 0; in_str = 0; raw = -1
  attr = ""; attr_sq = 0; hash = 0; modprefix = ""
  path = ""; path_line = 0; path_dot = 0
  prev = ""; gtop = 0; bk_n = 0; pending_mod = 0
  port = ""
  if (FILENAME ~ /^src\/application\/ports\/[a-z_]+\.rs$/ && FILENAME != "src/application/ports/mod.rs") {
    port = FILENAME; sub(/^.*\//, "", port); sub(/\.rs$/, "", port)
  }
}
function check(p, ln, dot,   seg) {
  if (p == "") return
  if (substr(p, 1, 2) == "::") { p = substr(p, 3); dot = 0 }
  if (port != "" && !dot && modprefix == "") {
    if (p == "crate::application::ports" || index(p, "crate::application::ports::") == 1)
      print "C8\t" FILENAME "\tcrate::application::ports\t" ln
    else if (index(p, "super::") == 1) {
      seg = substr(p, 8); sub(/::.*/, "", seg)
      if (seg == "super") {
        if (p == "super::super::ports" || index(p, "super::super::ports::") == 1)
          print "C8\t" FILENAME "\tsuper::super::ports\t" ln
      } else if (index(c8[port], "," seg ",") == 0) print "C8\t" FILENAME "\tsuper::" seg "\t" ln
    }
  }
}
function end_path() { check(path, path_line, path_dot); path = "" }
function finish_attribute(   expr, rest, part) {
  expr = attr; gsub(/[ \t]/, "", expr)
  # Attributes other than cfg can contain paths used by procedural macros.
  # Preserve their references rather than dropping code while collecting
  # attributes.
  if (expr !~ /^cfg\(/) {
    rest = attr
    code_token("[")
    while (match(rest, /[A-Za-z_][A-Za-z0-9_]*|::|[^ \t]/)) {
      part = substr(rest, RSTART, RLENGTH); rest = substr(rest, RSTART + RLENGTH)
      if (part != "\"") code_token(part)
    }
    code_token("]")
  }
  attr = ""; prev = ""; pending_mod = 0
}
# token handles one token of code (strings and comments already removed).
function token(tk) {
  if (attr_sq) {
    if (tk == "[") attr_sq++
    if (tk == "]") attr_sq--
    if (!attr_sq) finish_attribute()
    else attr = attr " " tk
    return
  }
  if (tk == "#") { end_path(); hash = 1; return }
  if (hash && tk == "!") return
  if (hash && tk == "[") { hash = 0; attr_sq = 1; attr = ""; return }
  hash = 0
  code_token(tk)
}
function code_token(tk,   is_ident) {
  is_ident = (tk ~ /^[A-Za-z_][A-Za-z0-9_]*$/)
  if (is_ident) {
    if (prev == "::" && path != "") path = path "::" tk
    else if (prev == "::") { path = "::" tk; path_line = FNR; path_dot = 0 }
    else {
      end_path()
      path = (gtop > 0 && (prev == "{" || prev == ",") && bk[bk_n] == "g") ? gprefix[gtop] "::" tk : tk
      path_line = FNR; path_dot = (prev == ".")
    }
    if (pending_mod == 1) { modname = tk; pending_mod = 2 }
    else pending_mod = (tk == "mod") ? 1 : 0
    prev = "ident"
    return
  }
  if (tk == "::") { prev = "::"; return }
  # A glob (`super::*`) ends its path with `*`, so C8 sees what it
  # names; one inside a group (`super::{*, a}`) globs the prefix of the group.
  if (tk == "*" && prev == "::" && path != "") path = path "::*"
  else if (tk == "*" && gtop > 0 && (prev == "{" || prev == ",") && bk[bk_n] == "g") {
    path = gprefix[gtop] "::*"; path_line = FNR; path_dot = 0
  }
  if (tk == "{" && prev == "::") {
    gtop++; gprefix[gtop] = path; path = ""
    bk[++bk_n] = "g"; prev = "{"; return
  }
  end_path()
  if (tk == "{") {
    bk[++bk_n] = "b"
    bk_prefix[bk_n] = modprefix
    if (pending_mod == 2) modprefix = modprefix modname "/"
  } else if (tk == "}") {
    if (bk_n > 0 && bk[bk_n] == "g") gtop--
    else modprefix = bk_prefix[bk_n]
    if (bk_n > 0) bk_n--
  }
  pending_mod = 0
  prev = tk
}
FNR == 1 { if (NR > 1) end_path(); reset_file() }
{
  line = $0; code = ""
  while (line != "") {
    if (in_block) {
      if (!match(line, /\/\*|\*\//)) { line = ""; break }
      tk = substr(line, RSTART, RLENGTH)
      line = substr(line, RSTART + RLENGTH)
      if (tk == "/*") in_block++
      else in_block--
      continue
    }
    if (in_str) {
      if (raw >= 0) {
        close_q = "\""; for (h = 0; h < raw; h++) close_q = close_q "#"
        i = index(line, close_q)
        if (i == 0) { line = ""; break }
        line = substr(line, i + length(close_q)); in_str = 0; raw = -1; code = code " \"\" "; continue
      }
      if (match(line, /^([^"\\]|\\.)*"/)) { line = substr(line, RLENGTH + 1); in_str = 0; code = code " \"\" "; continue }
      line = ""; break
    }
    if (!match(line, /b?r#*"|"|\/\/|\/\*|'\''/)) { code = code line; line = ""; break }
    code = code substr(line, 1, RSTART - 1)
    tk = substr(line, RSTART, RLENGTH); line = substr(line, RSTART + RLENGTH)
    if (tk == "//") { line = ""; break }
    if (tk == "/*") { in_block = 1; code = code " "; continue }
    if (tk == "'\''") {
      if (match(line, /^(\\.[^'\'']*|[\200-\377]+|[^'\''\\])'\''/)) { line = substr(line, RLENGTH + 1); code = code " " }
      else code = code " "
      continue
    }
    if (tk ~ /r/) { raw = gsub(/#/, "#", tk); in_str = 1; continue }
    in_str = 1; raw = -1
  }
  while (code != "") {
    if (!match(code, /[A-Za-z_][A-Za-z0-9_]*|::|[{};,.]|[^ \t]/)) break
    tk = substr(code, RSTART, RLENGTH); code = substr(code, RSTART + RLENGTH)
    if (tk == "\"" || tk == "'\''") continue
    token(tk)
  }
}
END { end_path() }
' $files
}

# check_tree checks the tree in the current directory against allow file $1.
check_tree() {
  allow=$1
  if [ ! -d src ]; then
    echo "$me: src not found under $(pwd)" >&2
    return 2
  fi
  found=$(scan)
  printf '%s\n' "$found" | LC_ALL=C awk -F '\t' -v me="$me" -v allow="$allow" -v show="${allow#"$repo"/}" '
    function trim(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); return s }
    BEGIN {
      bad = 0
      i = 0
      while ((getline l < allow) > 0) {
        i++
        if (l ~ /^[ \t]*(#|$)/) continue
        m = split(l, f, "|")
        if (m < 5) { print me ": " show ":" i ": want rule | path | reference | task | reason" > "/dev/stderr"; bad = 1; continue }
        rule = trim(f[1]); path = trim(f[2]); r = trim(f[3]); task = trim(f[4])
        reason = f[5]; for (j = 6; j <= m; j++) reason = reason "|" f[j]; reason = trim(reason)
        if (rule != "C8" || path !~ /^src\// || r == "" || task !~ /^[1-9][0-9]*( *, *[1-9][0-9]*)*$/ || reason == "") {
          print me ": " show ":" i ": want a rule (C8), a path under src/, a reference, task IDs (1234 or 1234, 1235) and a reason" > "/dev/stderr"; bad = 1; continue
        }
        k = rule "\t" path "\t" r
        if (k in item) { print me ": " show ":" i ": " rule " " path " " r " is listed twice" > "/dev/stderr"; bad = 1; continue }
        item[k] = i
      }
    }
    $1 == "" { next }
    {
      k = $1 "\t" $2 "\t" $3
      if (k in item) { used[k] = 1; next }
      print me ": " $2 ":" $4 ": " $1 " forbids " $3 "; fix it, or add \"" $1 " | " $2 " | " $3 " | <task> | <reason>\" to " show > "/dev/stderr"
      bad = 1
    }
    END {
      for (k in item) if (!(k in used)) {
        split(k, f, "\t")
        print me ": " show ":" item[k] ": stale item " f[1] " " f[2] " " f[3] " (no such reference any more); remove it" > "/dev/stderr"
        bad = 1
      }
      exit bad
    }'
}

self_test() {
  tmp_parent=${TMPDIR:-$repo/target}
  mkdir -p "$tmp_parent"
  tmp=$(mktemp -d "$tmp_parent/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT INT TERM
  fail=0
  expect() { # expect <want-exit> <name> <tree> [<text the output must have>]
    got=0
    (cd "$3" && check_tree allow.txt) >"$tmp/out" 2>&1 || got=$?
    if [ "$got" -ne "$1" ] || { [ -n "${4:-}" ] && ! grep -qF -- "$4" "$tmp/out"; }; then
      echo "$me --self-test: $2: exit $got, want $1${4:+ with \"$4\"}" >&2
      sed 's/^/  /' "$tmp/out" >&2
      fail=1
    else
      echo "$me --self-test: $2: exit $got as expected"
    fi
  }
  base() { # a tree with no violation
    d=$tmp/$1
    mkdir -p "$d/src/application/ports"
    cat >"$d/src/application/ports/mod.rs" <<'EOF'
mod observation;
mod planning;
mod shared;
pub use planning::*;
EOF
    cat >"$d/src/application/ports/planning.rs" <<'EOF'
//! Named in a comment only: super::execution::RunLog.
use super::shared::Clock; // super::host in a trailing comment
use super::super::GraphInput;
/// See crate::application::ports::Clock.
pub fn f() -> &'static str { "super::execution and crate::application::ports in a string" }
/* super::host
   in a block comment */
pub trait TaskStore { fn now(&self, clock: &dyn Clock); }
pub fn g(c: char) -> bool { c == '{' }
#[cfg(test)]
mod tests { use super::*; use super::TaskStore; }
EOF
    cat >"$d/src/application/ports/observation.rs" <<'EOF'
use super::{execution::RunLog, shared::Clock};
EOF
    : >"$d/allow.txt"
  }

  base clean
  expect 0 "C8: port modules name the modules they may use (comments, strings and inline modules are not counted)" "$tmp/clean"

  base nested_comments
  cat >>"$tmp/nested_comments/src/application/ports/planning.rs" <<'EOF'
/* outer /* inner */ super::execution::RunLog;
   /* another /* deeper */ crate::application::ports::Clock; */ still outer */
EOF
  expect 0 "nested_comments" "$tmp/nested_comments"

  base nested_comments_end
  cat >>"$tmp/nested_comments_end/src/application/ports/planning.rs" <<'EOF'
/* outer /* inner */ hidden */ fn f(_: &dyn super::execution::RunLog) {}
EOF
  expect 1 "nested_comments_end" "$tmp/nested_comments_end" "src/application/ports/planning.rs:12: C8 forbids super::execution"

  base attribute_ref
  cat >>"$tmp/attribute_ref/src/application/ports/planning.rs" <<'EOF'
#[super::host::custom_attribute]
fn production() {}
EOF
  expect 1 "attribute_ref" "$tmp/attribute_ref" "src/application/ports/planning.rs:12: C8 forbids super::host"

  base ports
  cat >>"$tmp/ports/src/application/ports/planning.rs" <<'EOF'
use super::{execution::RunLog, QueueRecords};
EOF
  expect 1 "C8: a port module names another context's module" "$tmp/ports" "src/application/ports/planning.rs:12: C8 forbids super::execution"
  expect 1 "C8: a port module names a port through the re-exports" "$tmp/ports" "src/application/ports/planning.rs:12: C8 forbids super::QueueRecords"
  echo 'fn f(_: &dyn crate::application::ports::Clock) {}' >>"$tmp/ports/src/application/ports/observation.rs"
  expect 1 "C8: a port module names crate::application::ports" "$tmp/ports" "src/application/ports/observation.rs:2: C8 forbids crate::application::ports"
  echo 'use super::*; use super::super::ports::Clock;' >>"$tmp/ports/src/application/ports/observation.rs"
  expect 1 "C8: a port module globs the re-exports" "$tmp/ports" "src/application/ports/observation.rs:3: C8 forbids super::*"
  expect 1 "C8: a port module names ports through super::super" "$tmp/ports" "src/application/ports/observation.rs:3: C8 forbids super::super::ports"

  base allowed
  echo 'use super::host::Host;' >>"$tmp/allowed/src/application/ports/planning.rs"
  echo 'C8 | src/application/ports/planning.rs | super::host | 1 | fixture' >"$tmp/allowed/allow.txt"
  expect 0 "violation in the allow list" "$tmp/allowed"

  base stale
  echo 'C8 | src/application/ports/planning.rs | super::host | 1 | fixture' >"$tmp/stale/allow.txt"
  expect 1 "stale item in the allow list" "$tmp/stale" "stale item C8 src/application/ports/planning.rs super::host"

  base notask
  echo 'use super::host::Host;' >>"$tmp/notask/src/application/ports/planning.rs"
  echo 'C8 | src/application/ports/planning.rs | super::host | | fixture' >"$tmp/notask/allow.txt"
  expect 1 "allow item without a task ID" "$tmp/notask" "allow.txt:1: want a rule"

  for rule in L2 L3; do
    base moved_rule
    echo "$rule | src/domain/run.rs | std::process | 1 | fixture" >"$tmp/moved_rule/allow.txt"
    expect 1 "allow item of a rule the Rust test checks: $rule" "$tmp/moved_rule" "allow.txt:1: want a rule"
  done

  rm -rf "$tmp"
  trap - EXIT INT TERM
  if [ -e "$tmp" ]; then echo "$me --self-test: $tmp was not removed" >&2; fail=1; fi
  [ "$fail" -eq 0 ] && echo "$me --self-test: ok"
  return "$fail"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

root=${LAYER_DEPS_ROOT:-$repo}
allow_file=${LAYER_DEPS_ALLOW_FILE:-$repo/.config/layer-deps-allow.txt}
cd "$root"
status=0
check_tree "$allow_file" || status=$?
[ "$status" -eq 0 ] && echo "$me: ok"
exit "$status"
