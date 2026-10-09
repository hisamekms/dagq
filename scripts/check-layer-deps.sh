#!/bin/sh
# Check the forbidden dependencies between the layers of the runtime, and
# between the split contexts of the supervisor, of the ports and of the
# composition root. The rules (their IDs L1, L2, L3, L4, L6, L7, L9, C3 and
# C8), what counts in short and the allow list's place are in
# docs/design/architecture.md, sections "レイヤーの規則", "コンテキストの規則"
# and "検査の範囲"; the allow list's format is in its own header
# (.config/layer-deps-allow.txt); the details of what counts are here.
#
# C3 counts in the files of $c3_files under src/application/supervise/ (the
# submodules that own 計画管理's, 観測と分析's and host運用's state): a
# reference to the loop's struct `Supervisor` or its `Slot` and `Phase`, or
# to the module `stages`, its states or the e2e's `E2eWaits`, through which
# the state of 実行と着地 is reached, is forbidden there, inside tests too.
# 実行と着地 lends its jobs and providers to 計画管理 through `JobDesk`,
# whose operations change them. It also counts in the
# files of $c3_stage_files (実行と着地's state by stage): a reference to
# `Supervisor` or to the module `contexts` or its states (the other
# contexts' state and the pass's view of the loop) is forbidden there.
# Each name counts bare, through `super::` and through
# `crate::application::supervise::`.
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
# L7 counts in src/compose.rs and every file under src/compose/ outside
# tests: a read of the wall or the monotonic clock (SystemTime::now,
# Instant::now). The modules of src/compose/ are found from the tree: each
# `name.rs` and `name/` right under it is the module `name`, so a new module
# is checked without a list. Each of them but those of $compose_loop is a
# context module. L9 counts, inside tests too, in the files of a context
# module: a reference to another module of src/compose/ (`super::host`,
# `crate::compose::host`), and a glob of the root (`super::*`, also inside
# a group as `super::{*, ..}`, in a file right under src/compose/ outside
# its inline modules, where `super` is the root; `crate::compose::*`
# anywhere), which would bring the other modules in unnamed; and in every
# file under src/
# outside src/compose.rs, src/compose/ and the three layers (src/main.rs,
# runtime.rs, lifecycle.rs, view, ...): a reference to any module of
# src/compose/ (`dagq::compose::host`), the root's re-exports being the way
# in.
#
# What counts is the path of a reference (`crate::application::timestamp`,
# `std::time::SystemTime::now`), with a grouped `use crate::{a, b}` expanded
# into one path each. Comments (`//`, `//!`, `///` and `/* */`, nested ones
# too), doc links and string and character literals do not count.
# A cfg is taken for test when its predicate requires test: `test` itself,
# an `all(...)` with any argument that requires test, or an `any(...)` all of
# whose arguments do (`#[cfg(test)]` over several lines, `cfg(any(test))`,
# `cfg(all(test, unix))`, `cfg(all(unix, any(test)))`). `cfg(any(test,
# unix))`, `not(...)` and any form not judged hold in a `test=false` build
# too, so they count as production (the safe side). The test range of a cfg
# is the item it is put on (a `mod tests { ... }`, a function, a `use`) to
# its end, and the files a test `mod name;` declares (`name.rs` and `name/`
# at the module's place, inline modules' names included); a test module
# inside an inline module is handled and the range ends where it ends. An
# inner `#![cfg(test)]` at the top of a file makes the whole file test.
# Inside a test range L1, L3 and L6 count and L2 and L4 do not.
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
# references only in comments and strings, nested block comments, the cfg
# forms above, a test range inside an inline module, the C3 references
# of a split context and of a stage, the C8 references and the L7 and L9
# references of the composition root, a new module of it and a glob of it)
# and removes them.
#
# Exit 0 when every occurrence is allowed and no item is stale, 1 when an
# occurrence is not allowed, an item is stale or the allow list is malformed
# (each goes to stderr), 2 when src/ is not found.
set -eu

me=check-layer-deps
# The submodules of src/application/supervise/ that rule C3 is checked on.
c3_files="contexts ci_watch main_log forecast live_alerts observer push report throughput_review cleanup disk host_metrics queue_service release sccache update sweep handoff inbox_nudge plan_review goal_review request_planner draft_planner finding_planner planner_turns"
# Rule C8: each port module of src/application/ports/ and the modules it may
# name, "module:allowed,allowed"; a port module not listed may name none.
c8_ok="planning:shared execution:shared host:shared,execution observation:shared,planning,execution,host shared:planning,execution,observation,host"
# The submodules that hold 実行と着地's state by stage, checked by C3 too.
c3_stage_files="stages"
# The modules of src/compose/ that are not a context module (rule L9): the
# supervisor loop's, which may name the contexts. Every other module of
# src/compose/ wires one context.
compose_loop="supervisor"
script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
repo=$(git rev-parse --show-toplevel 2>/dev/null) || repo=$(cd "$(dirname "$0")/.." && pwd)

# scan prints one line per occurrence: "rule<TAB>path<TAB>reference<TAB>line",
# with paths relative to the tree's root.
scan() {
  files=$(find src -type f -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  [ -n "$files" ] || return 0
  # The modules of src/compose/: each name.rs and name/ right under it.
  compose_modules=$(find src/compose -mindepth 1 -maxdepth 1 \( -name '*.rs' -o -type d \) 2>/dev/null | sed 's|^src/compose/||; s|\.rs$||' | LC_ALL=C sort -u | tr '\n' ' ')
  # shellcheck disable=SC2086
  LC_ALL=C awk -v c3_files="$c3_files" -v c3_stage_files="$c3_stage_files" -v c8_ok="$c8_ok" -v compose_modules="$compose_modules" -v compose_loop="$compose_loop" '
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
  n = split(c3_files, o, " ")
  for (i = 1; i <= n; i++) c3["src/application/supervise/" o[i] ".rs"] = 1
  n = split(c8_ok, o, " ")
  for (i = 1; i <= n; i++) { split(o[i], kv, ":"); c8[kv[1]] = "," kv[2] "," }
  n = split(c3_stage_files, o, " ")
  for (i = 1; i <= n; i++) c3s["src/application/supervise/" o[i] ".rs"] = 1
  c3_names = c3_patterns("Supervisor Slot Phase stages ClaimState SlotTable LandingState ResumeState TriageState ProviderState E2eWaits")
  c3s_names = c3_patterns("Supervisor contexts ObservationState HostOpsState PassEnv")
  prod["compose"] = "L7:SystemTime::now L7:Instant::now"
  prod["compose.rs"] = prod["compose"]
  n_cm = split(compose_modules, cm, " ")
  n = split(compose_loop, o, " ")
  for (i = 1; i <= n; i++) compose_loop_module[o[i]] = 1
  for (i = 1; i <= n_cm; i++) outside_compose = outside_compose (i > 1 ? " " : "") "L9:compose::" cm[i]
}
# c3_patterns prints the C3 patterns of each of the names: bare, through
# super:: and through crate::application::supervise::.
function c3_patterns(names,   m, i, nm, out) {
  m = split(names, nm, " "); out = ""
  for (i = 1; i <= m; i++) out = out " C3:^" nm[i] " C3:^super::" nm[i] " C3:^crate::application::supervise::" nm[i]
  return substr(out, 2)
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
  attr = ""; attr_sq = 0; hash = 0; inner_attr = 0; modprefix = ""
  path = ""; path_line = 0; path_test = 0; path_dot = 0
  prev = ""; gtop = 0; bk_n = 0; pending_mod = 0; sq = 0; item_sq = 0
  layer = FILENAME; sub(/^.*src\//, "", layer); sub(/\/.*/, "", layer)
  np = 0; load(ref[layer], 1); load(prod[layer], 0)
  port = ""; compose_ctx = 0
  if (FILENAME ~ /^src\/application\/ports\/[a-z_]+\.rs$/ && FILENAME != "src/application/ports/mod.rs") {
    port = FILENAME; sub(/^.*\//, "", port); sub(/\.rs$/, "", port)
  }
  if (layer == "compose") {
    module = FILENAME; sub(/^.*src\/compose\//, "", module); sub(/(\/.*|\.rs)$/, "", module)
    compose_ctx = !(module in compose_loop_module)
    if (compose_ctx)
      for (i = 1; i <= n_cm; i++) if (cm[i] != module) load("L9:^super::" cm[i] " L9:compose::" cm[i], 1)
  } else if (layer != "compose.rs" && layer != "domain" && layer != "application" && layer != "infrastructure") load(outside_compose, 1)
  if (FILENAME in c3) load(c3_names, 1)
  if (FILENAME in c3s) load(c3s_names, 1)
}
function check(p, ln, t, dot,   i, hit, seg) {
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
  if (compose_ctx && !dot) {
    if (p == "super::*" && modprefix == "" && FILENAME ~ /^src\/compose\/[^\/]+\.rs$/) print "L9\t" FILENAME "\tsuper::*\t" ln
    if (p == "crate::compose::*") print "L9\t" FILENAME "\tcrate::compose::*\t" ln
  }
  if (np == 0) return
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
# Prove only the supported predicates require test. Unknown/not predicates
# stay production. Empty any is always false; empty all requires no test.
function requires_test(expr,   op, body, level, start, j, c, child, result) {
  if (expr == "test") return 1
  if (expr !~ /^(all|any)\(/ || substr(expr, length(expr)) != ")") return 0
  op = substr(expr, 1, 3)
  body = substr(expr, 5, length(expr) - 5)
  level = 0; start = 1; result = (op == "any")
  for (j = 1; j <= length(body) + 1; j++) {
    c = substr(body, j, 1)
    if (c == "(") level++
    if (c == ")") level--
    if (level < 0) return 0
    if ((c == "," && level == 0) || j == length(body) + 1) {
      child = substr(body, start, j - start); start = j + 1
      if (child == "" && j == length(body) + 1) break
      if (child == "") return 0
      if (op == "all") result = result || requires_test(child)
      else result = result && requires_test(child)
    }
  }
  return level == 0 && result
}
function finish_attribute(   expr, rest, part) {
  expr = attr; gsub(/[ \t]/, "", expr)
  if (expr ~ /^cfg\(.*\)$/ && expr !~ /not\(/ && requires_test(substr(expr, 5, length(expr) - 5))) {
    if (inner_attr) {
      # Only file-level inner attributes classify the entire file.
      if (depth == 0) whole_test = 1
    } else if (!in_test && !whole_test) {
      in_test = 1; pending = 1; item_depth = depth; item_sq = sq
    }
  }
  # Other attributes can contain paths used by procedural macros. Preserve
  # their references rather than dropping code while collecting attributes.
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
  if (tk == "#") { end_path(); hash = 1; inner_attr = 0; return }
  if (hash && tk == "!") { inner_attr = 1; return }
  if (hash && tk == "[") { hash = 0; attr_sq = 1; attr = ""; return }
  hash = 0
  code_token(tk)
}
function code_token(tk,   is_ident) {
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
  # A glob (`super::*`) ends its path with `*`, so C8 and L9 see what it
  # names; one inside a group (`super::{*, a}`) globs the prefix of the group.
  if (tk == "*" && prev == "::" && path != "") path = path "::*"
  else if (tk == "*" && gtop > 0 && (prev == "{" || prev == ",") && bk[bk_n] == "g") {
    path = gprefix[gtop] "::*"; path_line = FNR; path_test = in_test || whole_test; path_dot = 0
  }
  if (tk == "{" && prev == "::") {
    gtop++; gprefix[gtop] = path; path = ""
    bk[++bk_n] = "g"; prev = "{"; return
  }
  end_path()
  if (tk == ";" && pending_mod == 2 && (in_test || whole_test)) print "testmod\t" FILENAME "\t" modprefix modname "\t" FNR
  if (tk == "{") {
    bk[++bk_n] = "b"; depth++
    bk_prefix[bk_n] = modprefix
    if (pending_mod == 2) modprefix = modprefix modname "/"
    if (pending) pending = 0
  } else if (tk == "}") {
    if (bk_n > 0 && bk[bk_n] == "g") gtop--
    else {
      modprefix = bk_prefix[bk_n]
      depth--
      if (in_test && (depth < item_depth || (!pending && depth == item_depth))) { in_test = 0; pending = 0 }
    }
    if (bk_n > 0) bk_n--
  } else if (tk == "[") sq++
  else if (tk == "]") sq--
  else if (tk == ";") {
    if (in_test && pending && depth == item_depth && sq == item_sq) { in_test = 0; pending = 0 }
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
      if ($1 == "L2" || $1 == "L4" || $1 == "L7") {
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
        if (rule !~ /^(L[123467]|L9|C[38])$/ || path !~ /^src\// || r == "" || task !~ /^[1-9][0-9]*( *, *[1-9][0-9]*)*$/ || reason == "") {
          print me ": " show ":" i ": want a rule (L1, L2, L3, L4, L6, L7, L9, C3, C8), a path under src/, a reference, task IDs (1234 or 1234, 1235) and a reason" > "/dev/stderr"; bad = 1; continue
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

  base nested_comments
  cat >>"$tmp/nested_comments/src/domain/run.rs" <<'EOF'
/* outer /* inner */ std::fs::read("hidden");
   /* another /* deeper */ crate::application::timestamp(); */ still outer */
EOF
  expect 0 "nested_comments" "$tmp/nested_comments"

  base nested_comments_end
  cat >>"$tmp/nested_comments_end/src/domain/run.rs" <<'EOF'
/* outer /* inner */ hidden */ std::fs::read("production");
EOF
  expect 1 "nested_comments_end" "$tmp/nested_comments_end" "L2 forbids std::fs"

  base multiline_cfg
  cat >>"$tmp/multiline_cfg/src/domain/run.rs" <<'EOF'
#[cfg(
    /* a nested /* comment */ here */ test
)]
fn multiline() { std::fs::read("test"); }
EOF
  expect 0 "multiline_cfg" "$tmp/multiline_cfg"

  base required_cfg
  cat >>"$tmp/required_cfg/src/domain/run.rs" <<'EOF'
#[cfg(any())]
fn unreachable() { std::fs::read("unreachable"); }
#[cfg(any(test))]
fn single_any() { std::fs::read("test"); }
#[cfg(all(test, unix))]
fn all_test() { std::fs::read("test"); }
#[cfg(all(unix, any(test)))]
fn nested() { std::fs::read("test"); }
#[cfg(any(all(test, unix), all(test, feature = "x"),))]
fn every_branch() { std::fs::read("test"); }
#[cfg(any(test))]
use std::fs;
EOF
  cat >>"$tmp/required_cfg/src/application/use_case.rs" <<'EOF'
#[cfg(all(test, unix))]
fn test_io() { std::fs::read("test"); }
EOF
  expect 0 "required_cfg (L2 and L4)" "$tmp/required_cfg"

  for predicate in 'any(test, unix)' 'any(test, feature = "x")' 'not(test)' 'all(unix, any(test, unix))' 'all(test, not(unix))' 'all()' 'feature = "test"'; do
    base production_cfg
    printf '#[cfg(%s)]\nfn production() { std::fs::read("production"); }\n' "$predicate" >>"$tmp/production_cfg/src/domain/run.rs"
    expect 1 "production_cfg: $predicate" "$tmp/production_cfg" "L2 forbids std::fs"
  done

  base inline_test_mod
  cat >>"$tmp/inline_test_mod/src/domain/run.rs" <<'EOF'
mod outer { #[cfg(test)] mod tests {
    fn io() { std::fs::read("test"); }
    #[cfg(test)] mod nested { fn io() { std::fs::read("test"); } }
    fn still_test() { std::fs::read("test"); }
} }
EOF
  expect 0 "inline_test_mod" "$tmp/inline_test_mod"

  base inline_test_mod_end
  cat >>"$tmp/inline_test_mod_end/src/domain/run.rs" <<'EOF'
mod outer { #[cfg(test)] mod tests { fn io() { std::fs::read("test"); } }
    fn production() { std::fs::read("production"); }
}
EOF
  expect 1 "inline_test_mod_end" "$tmp/inline_test_mod_end" "src/domain/run.rs:13: L2 forbids std::fs"

  base inline_test_file
  cat >>"$tmp/inline_test_file/src/domain/run.rs" <<'EOF'
mod outer { mod inner { #[cfg(test)] mod helpers; } }
EOF
  mkdir -p "$tmp/inline_test_file/src/domain/run/outer/inner/helpers"
  echo 'fn io() { std::fs::read("test"); }' >"$tmp/inline_test_file/src/domain/run/outer/inner/helpers.rs"
  echo 'fn io() { std::fs::read("test"); }' >"$tmp/inline_test_file/src/domain/run/outer/inner/helpers/child.rs"
  expect 0 "inline_test_file" "$tmp/inline_test_file"

  base inline_test_file_neighbor
  cat >>"$tmp/inline_test_file_neighbor/src/domain/run.rs" <<'EOF'
mod outer { #[cfg(test)] mod helpers; }
mod helpers;
EOF
  echo 'fn io() { std::fs::read("production"); }' >"$tmp/inline_test_file_neighbor/src/domain/run/helpers.rs"
  expect 1 "inline_test_file_neighbor" "$tmp/inline_test_file_neighbor" "src/domain/run/helpers.rs:1: L2 forbids std::fs"

  base multiline_cfg_end
  cat >>"$tmp/multiline_cfg_end/src/domain/run.rs" <<'EOF'
#[cfg(
    test
)]
fn test_io() { std::fs::read("test"); } fn production() { std::fs::read("production"); }
EOF
  expect 1 "multiline_cfg_end" "$tmp/multiline_cfg_end" "src/domain/run.rs:15: L2 forbids std::fs"

  base required_cfg_end
  cat >>"$tmp/required_cfg_end/src/domain/run.rs" <<'EOF'
#[cfg(all(test, unix))] use std::fs;
fn production() { std::fs::read("production"); }
EOF
  expect 1 "required_cfg_end" "$tmp/required_cfg_end" "src/domain/run.rs:13: L2 forbids std::fs"

  base required_cfg_ref
  cat >>"$tmp/required_cfg_ref/src/application/use_case.rs" <<'EOF'
#[cfg(any(test))] mod tests { use crate::infrastructure::sqlite; }
EOF
  expect 1 "required_cfg_ref (L3 still counts tests)" "$tmp/required_cfg_ref" "L3 forbids crate::infrastructure"

  base attribute_ref
  cat >>"$tmp/attribute_ref/src/domain/run.rs" <<'EOF'
#[crate::application::custom_attribute]
fn production() {}
EOF
  expect 1 "attribute_ref" "$tmp/attribute_ref" "L1 forbids crate::application"

  base context
  mkdir -p "$tmp/context/src/application/supervise"
  cat >"$tmp/context/src/application/supervise/report.rs" <<'EOF'
//! A pass of a split context; Supervisor is named in a comment only.
pub(super) fn pass(state: &mut ReportWatch, env: &mut PassEnv<'_>) {}
EOF
  cat >"$tmp/context/src/application/supervise/landing.rs" <<'EOF'
impl Supervisor<'_> { fn landing(&mut self) { self.slots.clear(); } }
EOF
  expect 0 "C3: a split context's file names no state of the loop" "$tmp/context"
  cat >>"$tmp/context/src/application/supervise/report.rs" <<'EOF'
impl Supervisor<'_> { fn slots(&self) -> usize { self.slots.len() } }
EOF
  expect 1 "C3: a split context's file reaches the loop's state" "$tmp/context" "src/application/supervise/report.rs:3: C3 forbids Supervisor"

  base ports
  mkdir -p "$tmp/ports/src/application/ports"
  cat >"$tmp/ports/src/application/ports/mod.rs" <<'EOF'
mod observation;
mod planning;
mod shared;
pub use planning::*;
EOF
  cat >"$tmp/ports/src/application/ports/planning.rs" <<'EOF'
//! Named in a comment only: super::execution::RunLog.
use super::shared::Clock;
use super::super::GraphInput;
pub trait TaskStore { fn now(&self, clock: &dyn Clock); }
#[cfg(test)]
mod tests { use super::*; use super::TaskStore; }
EOF
  cat >"$tmp/ports/src/application/ports/observation.rs" <<'EOF'
use super::{execution::RunLog, shared::Clock};
EOF
  expect 0 "C8: port modules name the modules they may use" "$tmp/ports"
  cat >>"$tmp/ports/src/application/ports/planning.rs" <<'EOF'
use super::{execution::RunLog, QueueRecords};
EOF
  expect 1 "C8: a port module names another context's module" "$tmp/ports" "src/application/ports/planning.rs:7: C8 forbids super::execution"
  expect 1 "C8: a port module names a port through the re-exports" "$tmp/ports" "src/application/ports/planning.rs:7: C8 forbids super::QueueRecords"
  echo 'fn f(_: &dyn crate::application::ports::Clock) {}' >>"$tmp/ports/src/application/ports/observation.rs"
  expect 1 "C8: a port module names crate::application::ports" "$tmp/ports" "src/application/ports/observation.rs:2: C8 forbids crate::application::ports"
  echo 'use super::*; use super::super::ports::Clock;' >>"$tmp/ports/src/application/ports/observation.rs"
  expect 1 "C8: a port module globs the re-exports" "$tmp/ports" "src/application/ports/observation.rs:3: C8 forbids super::*"
  expect 1 "C8: a port module names ports through super::super" "$tmp/ports" "src/application/ports/observation.rs:3: C8 forbids super::super::ports"

  base stage
  mkdir -p "$tmp/stage/src/application/supervise"
  cat >"$tmp/stage/src/application/supervise/stages.rs" <<'EOF2'
//! A stage's state; Supervisor and contexts are named in a comment only.
pub(super) struct SlotTable { slots: Vec<Slot> }
impl SlotTable { fn admit(&mut self, slot: Slot) { self.slots.push(slot); } }
EOF2
  cat >"$tmp/stage/src/application/supervise/report.rs" <<'EOF2'
pub(super) fn pass(state: &mut ReportWatch) {}
EOF2
  expect 0 "C3: a stage's file names only its own state and the slots" "$tmp/stage"
  cat >>"$tmp/stage/src/application/supervise/stages.rs" <<'EOF2'
fn cleaning(host: &super::contexts::HostOpsState) {}
EOF2
  expect 1 "C3: a stage's file reaches another context's state" "$tmp/stage" "src/application/supervise/stages.rs:4: C3 forbids super::contexts"
  cat >>"$tmp/stage/src/application/supervise/report.rs" <<'EOF2'
fn slots(claim: &ClaimState) -> usize { claim.slots.len() }
EOF2
  expect 1 "C3: a split context's file reaches a stage's state" "$tmp/stage" "src/application/supervise/report.rs:2: C3 forbids ClaimState"

  base compose
  mkdir -p "$tmp/compose/src/compose"
  cat >"$tmp/compose/src/compose.rs" <<'EOF'
mod host;
mod observation;
mod supervisor;
pub use host::up;
fn shared() { host::up(); }
EOF
  cat >"$tmp/compose/src/compose/host.rs" <<'EOF'
//! See super::observation::status, named in a comment only.
use super::{OneShot, shared};
pub fn up() { let _ = "crate::compose::observation"; shared(); }
EOF
  cat >"$tmp/compose/src/compose/observation.rs" <<'EOF'
pub fn status() { super::shared(); }
EOF
  cat >"$tmp/compose/src/compose/supervisor.rs" <<'EOF'
use super::{host, observation::status};
pub fn supervise() { host::up(); status(); }
EOF
  cat >"$tmp/compose/src/main.rs" <<'EOF'
use dagq::compose::{self, up};
fn main() { compose::status(); up(); }
EOF
  expect 0 "L9: the root and the supervisor loop name the contexts, the rest only the root" "$tmp/compose"
  cat >>"$tmp/compose/src/compose/observation.rs" <<'EOF'
use super::{host::up, OneShot};
EOF
  expect 1 "L9: a context module names another" "$tmp/compose" "src/compose/observation.rs:2: L9 forbids super::host"

  base compose_outside
  mkdir -p "$tmp/compose_outside/src/compose"
  echo 'pub fn up() {}' >"$tmp/compose_outside/src/compose/host.rs"
  echo 'fn main() { dagq::compose::host::up(); }' >"$tmp/compose_outside/src/main.rs"
  expect 1 "L9: outside the composition root a submodule is named" "$tmp/compose_outside" "src/main.rs:1: L9 forbids compose::host"

  base compose_clock
  mkdir -p "$tmp/compose_clock/src/compose"
  cat >"$tmp/compose_clock/src/compose/host.rs" <<'EOF'
#[cfg(test)]
fn fixture() { let _ = std::time::Instant::now(); }
pub fn up() { let _ = std::time::SystemTime::now(); }
EOF
  expect 1 "L7: the composition reads the clock outside tests" "$tmp/compose_clock" "src/compose/host.rs:3: L7 forbids SystemTime::now"

  base compose_new_module
  mkdir -p "$tmp/compose_new_module/src/compose/foo"
  printf 'mod foo;\nmod host;\n' >"$tmp/compose_new_module/src/compose.rs"
  echo 'pub fn up() {}' >"$tmp/compose_new_module/src/compose/host.rs"
  echo 'pub fn run() {}' >"$tmp/compose_new_module/src/compose/foo.rs"
  echo 'pub fn child() { super::run(); }' >"$tmp/compose_new_module/src/compose/foo/child.rs"
  expect 0 "L9: a new module that names only the root" "$tmp/compose_new_module"
  echo 'fn up() { super::host::up(); }' >>"$tmp/compose_new_module/src/compose/foo.rs"
  expect 1 "L9: a new module names another context's module" "$tmp/compose_new_module" "src/compose/foo.rs:2: L9 forbids super::host"
  echo 'fn up() { crate::compose::host::up(); }' >"$tmp/compose_new_module/src/compose/foo.rs"
  expect 1 "L9: a new module's submodule names another" "$tmp/compose_new_module" "src/compose/foo.rs:1: L9 forbids compose::host"
  echo 'fn main() { dagq::compose::foo::run(); }' >"$tmp/compose_new_module/src/main.rs"
  expect 1 "L9: outside the composition root a new module is named" "$tmp/compose_new_module" "src/main.rs:1: L9 forbids compose::foo"

  base compose_glob
  mkdir -p "$tmp/compose_glob/src/compose"
  printf 'mod host;\nmod observation;\nmod supervisor;\nstruct OneShot;\n' >"$tmp/compose_glob/src/compose.rs"
  echo 'pub fn up() {}' >"$tmp/compose_glob/src/compose/host.rs"
  cat >"$tmp/compose_glob/src/compose/observation.rs" <<'EOF'
//! use super::*; named in a comment only.
use super::OneShot;
pub fn status() {}
#[cfg(test)]
mod tests { use super::*; }
EOF
  echo 'use super::*; fn supervise() { host::up(); }' >"$tmp/compose_glob/src/compose/supervisor.rs"
  mkdir -p "$tmp/compose_glob/src/compose/observation"
  echo 'use super::*;' >"$tmp/compose_glob/src/compose/observation/helpers.rs"
  expect 0 "L9: the loop, a context's inline module and its submodule file glob their super" "$tmp/compose_glob"
  echo 'use super::*; fn f() { host::up(); }' >>"$tmp/compose_glob/src/compose/observation.rs"
  expect 1 "L9: a context module globs the root" "$tmp/compose_glob" "src/compose/observation.rs:6: L9 forbids super::*"
  echo 'use super::{OneShot, *};' >"$tmp/compose_glob/src/compose/host.rs"
  expect 1 "L9: a context module globs the root in a group" "$tmp/compose_glob" "src/compose/host.rs:1: L9 forbids super::*"
  echo '#[cfg(test)] mod tests { use crate::compose::*; }' >"$tmp/compose_glob/src/compose/host.rs"
  expect 1 "L9: a context module globs the root through crate" "$tmp/compose_glob" "src/compose/host.rs:1: L9 forbids crate::compose::*"

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
