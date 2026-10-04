#!/bin/sh
# Check the forbidden dependencies between the layers of the runtime. The
# rules (their IDs L1, L2, L3, L4 and L6), what counts (comments, strings,
# #[cfg(test)]) and the allow list's place and format are in
# docs/design/architecture.md, sections "レイヤーの規則" and "検査の範囲";
# this script only applies them and does not restate them.
#
# Every occurrence the rules forbid must have its (rule, path, reference) in
# the allow list (.config/layer-deps-allow.txt, or LAYER_DEPS_ALLOW_FILE),
# and every item of the allow list must still match an occurrence, so the
# task that fixes a violation also removes its item.
#
# Meant to be run from the repository root (`sh scripts/check-layer-deps.sh`).
# When run from anywhere else it changes to the repository root found from the
# script's own location. LAYER_DEPS_ROOT names another tree to check (its
# src/ is read); --self-test checks the script itself on small fixtures in a
# temporary directory under target/ and removes them.
#
# Exit 0 when every occurrence is allowed and no item is stale, 1 when an
# occurrence is not allowed, an item is stale or the allow list is malformed
# (each goes to stderr), 2 when src/ is not found.
set -eu

me=check-layer-deps
script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
repo=$(cd "$(dirname "$0")/.." && pwd)

# scan prints one line per occurrence: "rule<TAB>path<TAB>reference<TAB>line",
# with paths relative to the tree's root.
scan() {
  files=$(find src/domain src/application src/infrastructure -type f -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || return 0
  # shellcheck disable=SC2086
  LC_ALL=C awk '
BEGIN {
  # The patterns per layer, "rule:pattern"; a pattern starting with ^
  # matches at the start of a path, otherwise anywhere in it. The rules that
  # count inside #[cfg(test)] are in ref, the others in prod.
  outer = "crate::view crate::runtime crate::lifecycle"
  ref["domain"] = "L1:^crate::application L1:^crate::infrastructure L1:^crate::compose"
  ref["application"] = "L3:^crate::infrastructure L3:^crate::compose"
  ref["infrastructure"] = "L6:^crate::compose"
  n = split(outer, o, " ")
  for (i = 1; i <= n; i++) {
    ref["domain"] = ref["domain"] " L1:^" o[i]
    ref["application"] = ref["application"] " L3:^" o[i]
    ref["infrastructure"] = ref["infrastructure"] " L6:^" o[i]
  }
  prod["domain"] = "L2:^rusqlite L2:^std::fs L2:^std::process L2:^std::net L2:SystemTime::now L2:Instant::now L2:Uuid::new_v4 L2:^anyhow"
  prod["application"] = "L4:^rusqlite L4:^std::fs L4:process::Command L4:SystemTime::now L4:Uuid::new_v4"
}
function load(lst, test,   m, i, it) {
  m = split(lst, it, " ")
  for (i = 1; i <= m; i++) {
    np++
    p_rule[np] = substr(it[i], 1, 2); p_pat[np] = substr(it[i], 4); p_test[np] = test
    p_anch[np] = (substr(p_pat[np], 1, 1) == "^")
    if (p_anch[np]) p_pat[np] = substr(p_pat[np], 2)
  }
}
function reset_file() {
  depth = 0; in_block = 0; in_str = 0; raw = -1
  in_test = 0; pending = 0; item_depth = 0; whole_test = 0
  path = ""; path_line = 0; path_test = 0; path_dot = 0
  prev = ""; gtop = 0; bk_n = 0; pending_mod = 0; sq = 0; item_sq = 0
  layer = FILENAME; sub(/^.*src\//, "", layer); sub(/\/.*/, "", layer)
  np = 0; load(ref[layer], 1); load(prod[layer], 0)
}
function check(p, ln, t, dot,   i, hit) {
  if (p == "" || np == 0) return
  if (substr(p, 1, 2) == "::") { p = substr(p, 3); dot = 0 }
  for (i = 1; i <= np; i++) {
    if (t && !p_test[i]) continue
    if (p_anch[i]) {
      if (dot) continue
      hit = (p == p_pat[i] || index(p, p_pat[i] "::") == 1)
    } else hit = index("::" p "::", "::" p_pat[i] "::") > 0
    if (hit) print p_rule[i] "\t" FILENAME "\t" p_pat[i] "\t" ln
  }
}
function end_path() { check(path, path_line, path_test, path_dot); path = "" }
# token handles one token of code (strings and comments already removed).
function token(tk,   is_ident) {
  is_ident = (tk ~ /^[A-Za-z_][A-Za-z0-9_]*$/)
  if (is_ident) {
    if (prev == "::" && path != "") path = path "::" tk
    else if (prev == "::") { path = "::" tk; path_line = FNR; path_test = in_test || whole_test; path_dot = 0 }
    else {
      end_path()
      path = (gtop > 0 && (prev == "{" || prev == ",") && bk[bk_n] == "g") ? gprefix[gtop] "::" tk : tk
      path_line = FNR; path_test = in_test || whole_test; path_dot = (prev == ".")
    }
    if (pending_mod == 1) { modname = tk; pending_mod = 2 }
    else pending_mod = (tk == "mod") ? 1 : 0
    prev = "ident"
    return
  }
  if (tk == "::") { prev = "::"; return }
  if (tk == "{" && prev == "::") {
    gtop++; gprefix[gtop] = path; path = ""
    bk[++bk_n] = "g"; prev = "{"; return
  }
  end_path()
  if (tk == ";" && pending_mod == 2 && (in_test || whole_test)) print "testmod\t" FILENAME "\t" modname "\t" FNR
  pending_mod = 0
  if (tk == "{") {
    bk[++bk_n] = "b"; depth++
    if (pending) pending = 0
  } else if (tk == "}") {
    if (bk_n > 0 && bk[bk_n] == "g") gtop--
    else {
      depth--
      if (in_test && (depth < item_depth || (!pending && depth == item_depth))) { in_test = 0; pending = 0 }
    }
    if (bk_n > 0) bk_n--
  } else if (tk == "[") sq++
  else if (tk == "]") sq--
  else if (tk == ";") {
    if (in_test && pending && depth == item_depth && sq == item_sq) { in_test = 0; pending = 0 }
  }
  prev = tk
}
FNR == 1 { if (NR > 1) end_path(); reset_file() }
{
  line = $0; code = ""
  while (line != "") {
    if (in_block) {
      i = index(line, "*/")
      if (i == 0) { line = ""; break }
      line = substr(line, i + 2); in_block = 0; continue
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
    if (tk == "/*") { in_block = 1; continue }
    if (tk == "'\''") {
      if (match(line, /^(\\.[^'\'']*|[\200-\377]+|[^'\''\\])'\''/)) { line = substr(line, RLENGTH + 1); code = code " " }
      else code = code " "
      continue
    }
    if (tk ~ /r/) { raw = gsub(/#/, "#", tk); in_str = 1; continue }
    in_str = 1; raw = -1
  }
  if (code ~ /#!\[cfg\(test\)\]/) whole_test = 1
  if (code ~ /#\[cfg\(test\)\]/ && !in_test && !whole_test) { in_test = 1; pending = 1; item_depth = depth; item_sq = sq }
  sub(/#\[cfg\(test\)\]/, "", code)
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

# test_paths prints the file paths and directory prefixes declared as test
# modules (`#[cfg(test)] mod name;`) from the scan's testmod lines.
test_paths() {
  printf '%s\n' "$1" | awk -F '\t' '$1 == "testmod" {
    p = $2; n = $3
    if (p ~ /\/(mod|lib|main)\.rs$/) { d = p; sub(/\/[^\/]*$/, "", d) } else { d = p; sub(/\.rs$/, "", d) }
    print d "/" n ".rs"; print d "/" n "/"
  }'
}

# check_tree checks the tree in the current directory against allow file $1.
check_tree() {
  allow=$1
  if [ ! -d src ]; then
    echo "$me: src not found under $(pwd)" >&2
    return 2
  fi
  raw_out=$(scan)
  tests=$(test_paths "$raw_out")
  found=$(printf '%s\n' "$raw_out" | LAYER_DEPS_TESTS=$tests awk -F '\t' '
    BEGIN { n = split(ENVIRON["LAYER_DEPS_TESTS"], t, "\n") }
    $1 == "testmod" || $1 == "" { next }
    {
      if ($1 == "L2" || $1 == "L4") {
        for (i = 1; i <= n; i++) {
          if (t[i] == "") continue
          if ($2 == t[i] || (substr(t[i], length(t[i])) == "/" && index($2, t[i]) == 1)) next
        }
      }
      print
    }')
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
        if (rule !~ /^L[12346]$/ || path !~ /^src\// || r == "" || task !~ /^[1-9][0-9]*( *, *[1-9][0-9]*)*$/ || reason == "") {
          print me ": " show ":" i ": want a rule (L1, L2, L3, L4, L6), a path under src/, a reference, task IDs (1234 or 1234, 1235) and a reason" > "/dev/stderr"; bad = 1; continue
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
  mkdir -p "$repo/target"
  tmp=$(mktemp -d "$repo/target/$me-self-test.XXXXXX")
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
    mkdir -p "$d/src/domain/run" "$d/src/application" "$d/src/infrastructure"
    cat >"$d/src/domain/run.rs" <<'EOF'
//! crate::application is named here only in a comment.
use crate::domain::ids::TaskId; // crate::infrastructure in a trailing comment
/// See crate::compose::read_queue.
pub fn f() -> &'static str { "crate::application::x and std::fs in a string" }
/* crate::application
   in a block comment */
#[cfg(test)]
mod tests;
#[cfg(test)]
fn helper() { let _ = std::time::SystemTime::now(); }
pub fn g(c: char) -> bool { c == '{' }
EOF
    cat >"$d/src/domain/run/tests.rs" <<'EOF'
fn t() { let _ = std::fs::read("x"); }
EOF
    cat >"$d/src/application/use_case.rs" <<'EOF'
use crate::{
    domain::{ids::TaskId, run},
    application::ports::Clock,
};
pub fn now(clock: &dyn Clock) -> u64 { clock.now() }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = std::fs::read("fixture"); }
}
EOF
    cat >"$d/src/infrastructure/store.rs" <<'EOF'
use crate::application::ports::Clock;
pub fn open() { let _ = std::fs::read("x"); let _ = std::time::SystemTime::now(); }
EOF
    : >"$d/allow.txt"
  }

  base clean
  expect 0 "no violation (comments, strings and #[cfg(test)] I/O are not counted)" "$tmp/clean"

  base grouped
  cat >>"$tmp/grouped/src/application/use_case.rs" <<'EOF'
use crate::{domain::ids, infrastructure::sqlite};
EOF
  expect 1 "violation in a grouped use, not in the allow list" "$tmp/grouped" "src/application/use_case.rs:11: L3 forbids crate::infrastructure"

  base testref
  cat >>"$tmp/testref/src/domain/run/tests.rs" <<'EOF'
use crate::application::timestamp;
EOF
  expect 1 "L1 counts a reference from a test module file" "$tmp/testref" "src/domain/run/tests.rs:2: L1 forbids crate::application"

  base prodio
  cat >>"$tmp/prodio/src/domain/run.rs" <<'EOF'
pub fn h() -> u64 { std::time::SystemTime::now().elapsed().unwrap().as_secs() }
EOF
  expect 1 "L2 counts SystemTime::now outside #[cfg(test)]" "$tmp/prodio" "src/domain/run.rs:12: L2 forbids SystemTime::now"

  base cfgend
  cat >>"$tmp/cfgend/src/domain/run.rs" <<'EOF'
pub enum E { A, #[cfg(test)] Fake, }
pub fn k(c: char) -> bool { let _ = std::time::SystemTime::now(); c == 'é' }
EOF
  expect 1 "the #[cfg(test)] region ends with its enclosing item" "$tmp/cfgend" "src/domain/run.rs:13: L2 forbids SystemTime::now"

  base infra
  cat >>"$tmp/infra/src/infrastructure/store.rs" <<'EOF'
pub fn q() { crate::compose::read_queue(); }
EOF
  expect 1 "L6 counts crate::compose from infrastructure" "$tmp/infra" "src/infrastructure/store.rs:3: L6 forbids crate::compose"

  base allowed
  cat >>"$tmp/allowed/src/application/use_case.rs" <<'EOF'
pub fn p() -> &'static str { crate::view::task_detail() }
EOF
  echo 'L3 | src/application/use_case.rs | crate::view | 1 | fixture' >"$tmp/allowed/allow.txt"
  expect 0 "violation in the allow list" "$tmp/allowed"

  base stale
  echo 'L3 | src/application/use_case.rs | crate::view | 1 | fixture' >"$tmp/stale/allow.txt"
  expect 1 "stale item in the allow list" "$tmp/stale" "stale item L3 src/application/use_case.rs crate::view"

  base notask
  cat >>"$tmp/notask/src/application/use_case.rs" <<'EOF'
pub fn p() -> &'static str { crate::view::task_detail() }
EOF
  echo 'L3 | src/application/use_case.rs | crate::view | | fixture' >"$tmp/notask/allow.txt"
  expect 1 "allow item without a task ID" "$tmp/notask" "allow.txt:1: want a rule"

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
