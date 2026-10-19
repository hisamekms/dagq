#!/bin/sh
# Check the rule C5 of the runtime's context boundaries (its ID and meaning
# are in docs/design/architecture.md, section "コンテキストの規則"): no
# context names a port another context keeps internal. C1, C7 and X1 (the
# SQL of the store modules, and the ownership of the tables, the store
# modules and the transactions) are the Rust test src/context_boundaries.rs.
# Whether each port is public and to whom, and the context of the modules of
# src/application/ the paths do not tell, are in
# .config/context-boundaries.txt, whose header holds its format; the context
# of a store module of src/infrastructure/ is read from the
# `store("<path>", <Context>)` rows of src/context_boundaries/ownership.rs,
# one a line; the violations of today are in
# .config/context-boundaries-allow.txt, whose header holds its format. The
# details of what counts are here.
#
# The context of a file is derived from its path for
# src/application/ports/<context>.rs, src/application/queue_reads/<context>.rs
# and src/compose/<context>.rs, and read from the list's `module` items
# (under src/application/) and the Rust table's store rows (under
# src/infrastructure/) otherwise, the longest path winning. A file with
# neither has no context.
#
# The list C5 reads against the code ("C5 list"; these are not allowed in the
# allow list, the list is fixed instead):
#   - every pub (or pub(crate)) trait of src/application/ports/<context>.rs
#     (but mod.rs) has a `port` item, and every pub (or pub(crate)) trait of
#     src/application/ outside ports/ has an `outer-port` or a `not-port`
#     item at its file;
#   - every `port`, `outer-port`, `not-port`, `method`, `module` and `item`
#     item names something the code has: a trait of that file, a method
#     declared in the trait's body, a file or directory, a definition
#     (struct, enum, fn, type, const, static, mod, trait) in the file; a
#     definition in test code does not count;
#   - a `partial` port has at least one `method` item and a `method` item
#     names a `partial` port.
# C5: in a file with a context, a reference (an identifier in code) to a port
# owned by another context (not `shared`) is a violation unless the port is
# public to the file's context. A reference written with a path
# (`ports::RunRecovery`, not `self::`) or in a `use` (not of `self::`)
# always counts; a bare one does not when a production item of the type
# namespace of the same name (a struct, an enum, a union, a type, a mod, a
# trait; not a fn, a const or a static) is defined in a block around it: the file, the fn or block it is in, up to
# the nearest `mod` block, which sees no item of the blocks around it. An
# item in test code shadows nothing. A `partial` port counts as public, at the
# trait's level, to every context one of its `method` items names (so a
# partial port with a method public to `all` is never a C5 violation by
# itself). A file of `shared` (src/application/ports/shared.rs, whose Queue
# and StateStore join every port) is not counted. Its reference is the
# port's name. Unlike scripts/check-layer-deps.sh, references are not read
# inside strings.
#
# Comments (`//`, `/* */`, nested too) and string and character literals do
# not count. Test code does not count: an item under a
# `#[cfg(test)]` (or `#[cfg(all(test, ...))]`) line, a file under
# `#![cfg(test)]`, and the files a `#[cfg(test)] mod name;` declares
# (name.rs and name/ beside the declaring module). Items and fns are found
# by the start of their line (rustfmt's layout): `fn`, `trait` and the
# attributes at the start of a line after spaces and their visibility.
#
# The review keeps (the script does not see):
#   - the method-level reference to a `partial` port: whether a context
#     calls a method of the port that is not public to it (a call cannot be
#     tied to the trait by grep), and other contexts' use of ports through a
#     field or a variable without naming the trait;
#   - the files with no context (src/application/supervise.rs,
#     contexts.rs, execution.rs and execution/ (the loop and its stages),
#     the modules not in a `module` item, src/compose/supervisor.rs,
#     src/compose.rs, src/main.rs, src/domain/ and the rest);
#   - an item of another module named as a port and brought in by `use`
#     (the reference is taken for the port: the use path is not resolved);
#   - whom an `item` is lent to (only that it is defined is checked).
#
# Every violation must have its (rule, path, reference) in the allow list
# (.config/context-boundaries-allow.txt, or CONTEXT_BOUNDARIES_ALLOW_FILE),
# and every item of the allow list must still match a violation, so the task
# that fixes a violation also removes its item.
#
# The tree checked, and the lists read, are those of the git work tree of the
# cwd (`git rev-parse --show-toplevel`), so a copy of the script run
# elsewhere with the cwd in a worktree checks that worktree (the program
# review of a run; docs/development/task-registration.md, section
# "推奨の組み合わせ"). Outside a git work tree it is the repository found
# from the script's own location. Run it from the repository root
# (`sh scripts/check-context-boundaries.sh`). CONTEXT_BOUNDARIES_ROOT names
# another tree to check, CONTEXT_BOUNDARIES_LIST_FILE another list,
# CONTEXT_BOUNDARIES_ALLOW_FILE another allow list and
# CONTEXT_BOUNDARIES_STORES_FILE another Rust table of the store rows;
# --self-test checks the script itself on small fixtures in a temporary
# directory under ${TMPDIR:-target/} and removes them.
#
# Exit 0 when the list matches the code, every violation is allowed and no
# allow item is stale; 1 when not, or a list is malformed (each goes to
# stderr); 2 when src/, the list or the Rust table is not found.
set -eu

me=check-context-boundaries
script=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
repo=$(git rev-parse --show-toplevel 2>/dev/null) || repo=$(cd "$(dirname "$0")/.." && pwd)

# test_files prints the files and directories (ending in /) a
# `#[cfg(test)] mod name;` declares, one per line.
test_files() {
  LC_ALL=C grep -E -A2 '^[[:space:]]*#\[cfg\((test|all\(test[,)].*)\)\]' "$@" 2>/dev/null | LC_ALL=C awk '
    $0 == "--" { pend = 0; next }
    match($0, /\.rs[-:]/) {
      file = substr($0, 1, RSTART + 2); text = substr($0, RSTART + RLENGTH)
      if (substr($0, RSTART + 3, 1) == ":" && text ~ /^[ \t]*#\[cfg\(/) { pend = 1; cur = file }
      if (!pend || file != cur) next
      if (match(text, /mod[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*;/)) {
        n = substr(text, RSTART, RLENGTH); sub(/^mod[ \t]+/, "", n); sub(/[ \t]*;$/, "", n)
        p = file
        if (p ~ /\/(mod|lib|main)\.rs$/) sub(/\/[^\/]*$/, "", p); else sub(/\.rs$/, "", p)
        print p "/" n ".rs"; print p "/" n "/"
        pend = 0
      } else if (text !~ /^[ \t]*(#|$)/) pend = 0
    }' || true
}

# check_tree checks the tree in the current directory against list $1,
# allow list $2 and the store rows of the Rust table $3.
check_tree() {
  list=$1 allow=$2 stores=$3
  if [ ! -d src ]; then
    echo "$me: src not found under $(pwd)" >&2
    return 2
  fi
  for f in "$list" "$stores"; do
    if [ ! -f "$f" ]; then
      echo "$me: $f not found" >&2
      return 2
    fi
  done
  [ -f "$allow" ] || allow=/dev/null
  files=$(find src/application src/infrastructure src/compose -type f -name '*.rs' 2>/dev/null | LC_ALL=C sort)
  # The files of the list's items outside those directories (definitions).
  extra=$(LC_ALL=C awk -F '|' '$1 ~ /^[ \t]*item[ \t]*$/ { p = $2; gsub(/^[ \t]+|[ \t]+$/, "", p); print p }' "$list" |
    grep -vE '^src/(application|infrastructure|compose)/' | LC_ALL=C sort -u |
    while read -r p; do if [ -f "$p" ]; then echo "$p"; fi; done)
  tests=""
  # shellcheck disable=SC2086
  if [ -n "$files" ]; then tests=$(test_files $files); fi
  # shellcheck disable=SC2086
  CB_TESTS=$tests LC_ALL=C awk -v me="$me" -v list="$list" -v allow="$allow" -v stores="$stores" \
    -v show_list="${list#"$repo"/}" -v show_allow="${allow#"$repo"/}" -v show_stores="${stores#"$repo"/}" '
function trim(s) { gsub(/^[ \t]+|[ \t]+$/, "", s); return s }
function err(msg) { bad = 1; print me ": " msg | "sort -u 1>&2" }
function lerr(i, msg) { err(show_list ":" i ": " msg) }
function is_ctx(c) { return (c in ctxs) }
# good_to tells whether t is a `to` field: all, -, or contexts and ,.
function good_to(t,   m, i, a) {
  if (t == "all" || t == "-") return 1
  m = split(t, a, ",")
  for (i = 1; i <= m; i++) if (!is_ctx(trim(a[i]))) return 0
  return m > 0
}
# add_to adds the contexts of the `to` field t to whom port p is public.
function add_to(p, t,   m, i, a) {
  if (t == "-") return
  if (t == "all") { pto[p] = pto[p] " all "; return }
  m = split(t, a, ",")
  for (i = 1; i <= m; i++) pto[p] = pto[p] " " trim(a[i]) " "
}
# derived prints the context the path tells, or "".
function derived(path,   c) {
  if (path ~ /^src\/application\/(ports|queue_reads)\/[a-z_]+\.rs$/ || path ~ /^src\/compose\/[a-z_]+\.rs$/) {
    c = path; sub(/^.*\//, "", c); sub(/\.rs$/, "", c)
    if (is_ctx(c)) return c
  }
  return ""
}
function covers(k, path) { return path == k || (substr(k, length(k)) == "/" && index(path, k) == 1) }
function ctx_of(path,   c, best, k) {
  c = derived(path)
  if (c != "") return c
  best = ""
  for (k in place) if (covers(k, path) && length(k) > length(best)) best = k
  return best == "" ? "" : place[best]
}
function enough(i, m, n, what) {
  if (m >= n) return 1
  lerr(i, "want " what)
  return 0
}
function read_list(   i, l, m, j, f, kind, p, v, t, o, k) {
  i = 0
  while ((getline l < list) > 0) {
    i++
    if (l ~ /^[ \t]*(#|$)/) continue
    m = split(l, f, "|")
    for (j = 1; j <= m; j++) f[j] = trim(f[j])
    kind = f[1]
    if (kind == "module") {
      if (!enough(i, m, 4, kind " | <path> | <context> | <note>")) continue
      if (!is_ctx(f[3])) { lerr(i, "unknown context " f[3]); continue }
      if (f[2] !~ /^src\/application\//) { lerr(i, "a module is under src/application/"); continue }
      if (derived(f[2]) != "") { lerr(i, f[2] " is " derived(f[2]) " by its path; remove the item"); continue }
      if (f[2] in place) { lerr(i, f[2] " is listed twice"); continue }
      place[f[2]] = f[3]; plline[f[2]] = i
    } else if (kind == "port" || kind == "outer-port") {
      if (kind == "port") {
        if (!enough(i, m, 5, "port | <Trait> | <public|internal|partial> | <to> | <note>")) continue
        p = f[2]; v = f[3]; t = f[4]; o = ""
      } else {
        if (!enough(i, m, 7, "outer-port | <path> | <Trait> | <owner> | <public|internal|partial> | <to> | <note>")) continue
        p = f[3]; v = f[5]; t = f[6]; o = f[4]
        if (f[2] ~ /^src\/application\/ports\//) { lerr(i, p " is in ports/; make it a port item"); continue }
        if (o == "-") {
          o = derived(f[2])
          if (o == "") { lerr(i, "the path " f[2] " does not tell the owner of " p "; write it"); continue }
        } else if (!is_ctx(o)) { lerr(i, "unknown context " o); continue }
        else if (derived(f[2]) != "") { lerr(i, "the path " f[2] " tells the owner of " p "; write -"); continue }
      }
      if (p in pkind) { lerr(i, p " is listed twice"); continue }
      if (v != "public" && v != "internal" && v != "partial") { lerr(i, "want public, internal or partial, not " v); continue }
      if (!good_to(t)) { lerr(i, "want all, - or contexts for to, not " t); continue }
      if ((v == "public") == (t == "-")) { lerr(i, v " takes " (v == "public" ? "all or contexts" : "-") " for to"); continue }
      pkind[p] = kind; pvis[p] = v; pline[p] = i; pown[p] = o; pto[p] = ""
      if (kind == "outer-port") ppath[p] = f[2]
      add_to(p, t)
    } else if (kind == "not-port") {
      if (!enough(i, m, 4, "not-port | <path> | <Trait> | <reason>")) continue
      if (f[4] == "") { lerr(i, "a not-port needs a reason"); continue }
      if (f[3] in pkind) { lerr(i, f[3] " is listed twice"); continue }
      pkind[f[3]] = kind; ppath[f[3]] = f[2]; pline[f[3]] = i
    } else if (kind == "method") {
      if (!enough(i, m, 5, "method | <Trait> | <method> | <to> | <note>")) continue
      if (!good_to(f[4]) || f[4] == "-") { lerr(i, "want all or contexts for to, not " f[4]); continue }
      k = f[2] SUBSEP f[3]
      if (k in mline) { lerr(i, "method " f[2] "::" f[3] " is listed twice"); continue }
      mline[k] = i; mto[k] = f[4]
    } else if (kind == "item") {
      if (!enough(i, m, 6, "item | <path> | <name> | <owner> | <to> | <note>")) continue
      if (!is_ctx(f[4])) { lerr(i, "unknown context " f[4]); continue }
      if (!good_to(f[5]) || f[5] == "-") { lerr(i, "want all or contexts for to, not " f[5]); continue }
      k = f[2] SUBSEP f[3]
      if (k in iline) { lerr(i, "item " f[3] " of " f[2] " is listed twice"); continue }
      iline[k] = i; deffile[f[2]] = 1
    } else lerr(i, "unknown kind " kind)
  }
  close(list)
}
# read_stores reads the context of the store modules of src/infrastructure/
# from the `store("<path>", <Context>)` rows of the Rust table, one a line.
function read_stores(   l, p, c, n) {
  n = 0
  while ((getline l < stores) > 0) {
    if (l !~ /(^|[^A-Za-z0-9_])store\(/ || l ~ /^[ \t]*(\/\/|const fn store)/) continue
    if (l !~ /^[ \t]*store\("src\/infrastructure\/[^"]*", *[A-Z][a-z]+\)/) { err(show_stores ": a store row the script cannot read (want store(\"src/infrastructure/...\", Context) on one line): " l); continue }
    p = l; sub(/^[ \t]*store\("/, "", p); c = p; sub(/".*$/, "", p)
    sub(/^[^"]*", */, "", c); sub(/\).*$/, "", c); c = tolower(c)
    if (!is_ctx(c)) { err(show_stores ": unknown context " c " of " p); continue }
    place[p] = c; n++
  }
  close(stores)
  if (n == 0) err(show_stores ": no store(...) row; the Rust table holds them one a line")
}
function read_allow(   i, l, m, f, j, rule, path, r, task, reason, k) {
  i = 0
  while ((getline l < allow) > 0) {
    i++
    if (l ~ /^[ \t]*(#|$)/) continue
    m = split(l, f, "|")
    if (m < 5) { err(show_allow ":" i ": want rule | path | reference | task | reason"); continue }
    rule = trim(f[1]); path = trim(f[2]); r = trim(f[3]); task = trim(f[4])
    reason = f[5]; for (j = 6; j <= m; j++) reason = reason "|" f[j]; reason = trim(reason)
    if (rule != "C5" || path !~ /^src\// || r == "" || task !~ /^(none|[1-9][0-9]*( *, *[1-9][0-9]*)*)$/ || reason == "") {
      err(show_allow ":" i ": want a rule (C5), a path under src/, a reference, task IDs (1234 or 1234, 1235) or none, and a reason"); continue
    }
    k = rule SUBSEP path SUBSEP r
    if (k in item) { err(show_allow ":" i ": " rule " " path " " r " is listed twice"); continue }
    item[k] = i
  }
  close(allow)
}
BEGIN {
  bad = 0
  q = sprintf("%c", 39)
  open_re = "b?r#*\"|\"|//|/\\*|" q
  char_re = "^(\\\\.[^" q "]*|[\200-\377]+|[^" q "\\\\])" q
  n = split("planning execution observation host shared", a, " ")
  for (i = 1; i <= n; i++) ctxs[a[i]] = 1
  n = split(ENVIRON["CB_TESTS"], a, "\n")
  for (i = 1; i <= n; i++) if (a[i] != "") testpath[a[i]] = 1
  read_list()
  read_stores()
  read_allow()
  for (k in mline) {
    split(k, a, SUBSEP)
    if (!(a[1] in pvis) || pvis[a[1]] != "partial") lerr(mline[k], "method " a[1] "::" a[2] " names " a[1] ", which is not a partial port")
    else { nmeth[a[1]]++; add_to(a[1], mto[k]) }
  }
  for (p in pvis) if (pvis[p] == "partial" && !(p in nmeth)) lerr(pline[p], "the partial port " p " has no method item")
  # One regex for a reference to any port.
  pre = ""
  for (p in pvis) pre = pre (pre == "" ? "" : "|") p
  pre = (pre == "") ? "^$x" : "(^|[^A-Za-z0-9_])(" pre ")($|[^A-Za-z0-9_])"
}
# testing tells whether the code read now is test code: the whole file, an
# item under a test cfg, or the start of one before its `{` or `;`.
function testing() { return whole_test || file_test || ntest > 0 || pend_test }
# refs keeps the references to ports in the code c, judged at the end
# (the owner of a port of ports/ is known once its file is read).
# A reference written with a path (`ports::RunRecovery`, not `self::`) or
# in a `use` (not of `self::`) always counts; a bare one is kept with the
# chain of its enclosing scopes: a production item of the type namespace of
# the same name in one of them shadows it.
function refs(c,   id, before, k) {
  if (testing()) return
  while (match(c, pre)) {
    id = substr(c, RSTART, RLENGTH); before = substr(c, 1, RSTART - 1); c = substr(c, RSTART + RLENGTH)
    if (id ~ /^[^A-Za-z0-9_]/) { before = before substr(id, 1, 1); id = substr(id, 2) }
    if (id ~ /[^A-Za-z0-9_]$/) { c = substr(id, length(id)) c; id = substr(id, 1, length(id) - 1) }
    k = FILENAME SUBSEP id
    rctx[FILENAME] = fctx
    if ((in_use && !use_self) || (before ~ /::[ \t]*$/ && before !~ /(^|[^A-Za-z0-9_])(self|Self)[ \t]*::[ \t]*$/)) { if (!(k in refq)) refq[k] = FNR }
    else { k = k SUBSEP chain[depth]; if (!(k in refb)) refb[k] = FNR }
  }
}
# code handles the code c (no comment, no string) of the current line.
function code(c,   ch, seg) {
  if (c == "") return
  if (pend_fn == "" && pend_trait == "" && !pend_test && !pend_mod && !in_use && c !~ /[{}]/) {
    if (fctx != "" && c ~ pre) refs(c)
    if (c ~ /[][()]/) pl += gsub(/[([]/, "", c) - gsub(/[])]/, "", c)
    return
  }
  while (match(c, /[{};,]/)) {
    ch = substr(c, RSTART, 1); seg = substr(c, 1, RSTART - 1); c = substr(c, RSTART + 1)
    if (fctx != "" && seg ~ pre) refs(seg)
    if (seg ~ /[][()]/) pl += gsub(/[([]/, "", seg) - gsub(/[])]/, "", seg)
    if (ch == "{") {
      depth++; sid++; scid[depth] = sid
      # A module block sees no item of the blocks around it.
      chain[depth] = pend_mod ? sid : chain[depth - 1] "/" sid; pend_mod = 0
      if (pend_test) { nf++; fk[nf] = "test"; fd[nf] = depth; pend_test = 0; ntest++ }
      if (pend_trait != "") { nf++; fk[nf] = "trait"; fd[nf] = depth; fname[nf] = pend_trait; fdl[nf] = pend_trait_line; pend_trait = "" }
      else if (pend_fn != "") {
        if (nf > 0 && fk[nf] == "trait" && fd[nf] == depth - 1 && !testing()) tmeth[FILENAME SUBSEP fname[nf] SUBSEP fdl[nf] SUBSEP pend_fn] = 1
        nf++; fk[nf] = "fn"; fd[nf] = depth; fname[nf] = pend_fn; pend_fn = ""
      }
    } else if (ch == "}") {
      while (nf > 0 && fd[nf] == depth) { if (fk[nf] == "test") ntest--; nf-- }
      depth--
      if (pend_test && depth < test_depth) pend_test = 0
    } else if (pl <= 0) {
      if (ch == ";") {
        if (pend_fn != "" && nf > 0 && fk[nf] == "trait" && fd[nf] == depth && !testing()) tmeth[FILENAME SUBSEP fname[nf] SUBSEP fdl[nf] SUBSEP pend_fn] = 1
        pend_fn = ""; pend_trait = ""; pend_test = 0; pend_mod = 0; in_use = 0
      } else if (pend_test && !test_item && depth == test_depth) pend_test = 0
    }
  }
  if (fctx != "" && c ~ pre) refs(c)
  if (c ~ /[][()]/) pl += gsub(/[([]/, "", c) - gsub(/[])]/, "", c)
}
FNR == 1 {
  depth = 0; nf = 0; ntest = 0; pl = 0; in_block = 0; in_str = 0; raw = -1
  pend_fn = ""; pend_trait = ""; pend_test = 0; test_item = 0; whole_test = 0; pend_mod = 0; in_use = 0
  sid++; scid[0] = sid; chain[0] = sid
  seen[FILENAME] = 1
  file_test = 0
  for (k in testpath) if (covers(k, FILENAME)) file_test = 1
  fctx = ctx_of(FILENAME)
  needdef = (FILENAME in deffile)
}
{
  line = $0
  if (!in_str && !in_block) {
    if (index(line, "use ") && line ~ /^[ \t]*(pub(\([^)]*\))?[ \t]+)?use[ \t]/) { in_use = 1; use_self = (line ~ /use[ \t]+(self|Self)[ \t]*::/) }
    if (index(line, "cfg(")) {
      if (depth == 0 && line ~ /^[ \t]*#!\[cfg\((test|all\(test[,)].*)\)\]/) whole_test = 1
      if (line ~ /^[ \t]*#\[cfg\((test|all\(test[,)].*)\)\]/) { pend_test = 1; test_item = 0; test_depth = depth }
    } else if (pend_test && line ~ /^[ \t]*(pub(\([^)]*\))?[ \t]+)?(fn|impl|mod|struct|enum|union|trait|const|static|type|use|unsafe|async|extern|macro_rules!)([^A-Za-z0-9_]|$)/) test_item = 1
    if (index(line, "trait ") && match(line, /^[ \t]*(pub(\([^)]*\))?[ \t]+)?(unsafe[ \t]+)?trait[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
      t = substr(line, RSTART, RLENGTH); sub(/^.*trait[ \t]+/, "", t); pend_trait = t; pend_trait_line = FNR
      if (!testing()) defs[FILENAME SUBSEP t] = 1
      if ((t in pvis) && !testing()) localdef[scid[depth] SUBSEP t] = 1
      if (line ~ /^[ \t]*pub(\(crate\))?[ \t]+(unsafe[ \t]+)?trait/ && !testing() && !((FILENAME SUBSEP t) in tdef)) tdef[FILENAME SUBSEP t] = FNR
    } else if (index(line, "fn ") && match(line, /^[ \t]*(pub(\([^)]*\))?[ \t]+)?((const|async|unsafe|extern[ \t]+"[^"]*")[ \t]+)*fn[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
      t = substr(line, RSTART, RLENGTH); sub(/^.*fn[ \t]+/, "", t); pend_fn = t
      if (needdef && !testing()) defs[FILENAME SUBSEP t] = 1
    } else if (match(line, /^[ \t]*(pub(\([^)]*\))?[ \t]+)?(struct|enum|union|type|const|static|mod)[ \t]+[A-Za-z_][A-Za-z0-9_]*/)) {
      t = substr(line, RSTART, RLENGTH); kind = t; sub(/^.*[ \t]/, "", t)
      if (kind ~ /mod[ \t]+[A-Za-z_0-9]*$/) pend_mod = 1
      if (needdef && !testing()) defs[FILENAME SUBSEP t] = 1
      # Only an item of the type namespace (not a const or a static) hides
      # a trait of its name.
      if ((t in pvis) && !testing() && kind ~ /(struct|enum|union|type|mod)[ \t]+[A-Za-z_0-9]*$/) localdef[scid[depth] SUBSEP t] = 1
    }
  }
  while (line != "") {
    if (in_block) {
      if (!match(line, /\/\*|\*\//)) { line = ""; break }
      tk = substr(line, RSTART, RLENGTH); line = substr(line, RSTART + RLENGTH)
      if (tk == "/*") in_block++; else in_block--
      continue
    }
    if (in_str) {
      if (raw >= 0) {
        cq = "\""; for (h = 0; h < raw; h++) cq = cq "#"
        i = index(line, cq)
        if (i == 0) { line = ""; break }
        line = substr(line, i + length(cq)); in_str = 0; raw = -1; continue
      }
      if (match(line, /^([^"\\]|\\.)*"/)) { line = substr(line, RLENGTH + 1); in_str = 0; continue }
      line = ""; break
    }
    if (!match(line, open_re)) { code(line); line = ""; break }
    rs = RSTART; rl = RLENGTH
    code(substr(line, 1, rs - 1))
    tk = substr(line, rs, rl); line = substr(line, rs + rl)
    if (tk == "//") { line = ""; break }
    if (tk == "/*") { in_block = 1; continue }
    if (tk == q) {
      if (match(line, char_re)) line = substr(line, RLENGTH + 1)
      continue
    }
    if (tk ~ /r/) { raw = gsub(/#/, "#", tk); in_str = 1; continue }
    in_str = 1; raw = -1
  }
}
END {
  # C5 list: the traits of the code against the list.
  # Each definition (file and name) is matched on its own, so a trait of
  # the same name as a listed one elsewhere still needs its own item.
  for (k in tdef) {
    split(k, a, SUBSEP); f = a[1]; t = a[2]; at = f ":" tdef[k] ": C5 list: "
    if (f ~ /^src\/application\/ports\/[a-z_]+\.rs$/ && f != "src/application/ports/mod.rs") {
      if (!(t in pkind)) err(at "the port " t " has no item in " show_list "; add \"port | " t " | <public|internal|partial> | <to> | <note>\"")
      else if (pkind[t] != "port") err(at "the port " t " is in ports/; make " show_list ":" pline[t] " a port item")
      else if (t in pfile) err(at "the port " t " is defined in " pfile[t] " too")
      else { pfile[t] = f; pown[t] = derived(f) }
    } else if (f ~ /^src\/application\// && f !~ /^src\/application\/ports\//) {
      if (!(t in pkind) || pkind[t] == "port" || ppath[t] != f)
        err(at "the pub trait " t " outside ports/ has no item for " f " in " show_list "; add an outer-port or a not-port item")
    }
  }
  for (p in pkind) {
    if (pkind[p] != "port") pfile[p] = ppath[p]
    if (pkind[p] == "port" && !(p in pfile)) err(show_list ":" pline[p] ": C5 list: the port " p " is no pub trait of src/application/ports/<context>.rs")
    else if (pkind[p] != "port" && !((ppath[p] SUBSEP p) in tdef)) err(show_list ":" pline[p] ": C5 list: " p " is no pub trait of " ppath[p])
  }
  for (k in mline) {
    split(k, a, SUBSEP)
    # A method counts only in the body of the production declaration of
    # the port (its file and line), outside test code.
    if (pvis[a[1]] == "partial" && (a[1] in pfile) && ((pfile[a[1]] SUBSEP a[1]) in tdef) && !((pfile[a[1]] SUBSEP a[1] SUBSEP tdef[pfile[a[1]] SUBSEP a[1]] SUBSEP a[2]) in tmeth)) err(show_list ":" mline[k] ": C5 list: the trait " a[1] " declares no method " a[2])
  }
  for (k in place) {
    ok = 0
    for (f in seen) if (covers(k, f)) { ok = 1; break }
    if (!ok && (k in plline)) err(show_list ":" plline[k] ": C5 list: " k " is no Rust file or directory of one")
  }
  for (k in iline) if (!(k in defs)) { split(k, a, SUBSEP); err(show_list ":" iline[k] ": C5 list: " a[1] " does not define " a[2]) }
  # C5.
  # A bare reference counts unless a production item of its name in one of
  # its enclosing scopes shadows it; a reference with a path always counts.
  for (k in refb) {
    split(k, a, SUBSEP); n = split(a[3], sc, "/"); shadowed = 0
    for (i = 1; i <= n; i++) if ((sc[i] SUBSEP a[2]) in localdef) { shadowed = 1; break }
    if (shadowed) continue
    r = a[1] SUBSEP a[2]
    if (!(r in refq) || refb[k] < refq[r]) refq[r] = refb[k]
  }
  for (k in refq) {
    split(k, a, SUBSEP); f = a[1]; p = a[2]; c = rctx[f]; o = pown[p]
    if (o == "" || o == c || o == "shared" || c == "shared") continue
    if (index(pto[p], " all ") || index(pto[p], " " c " ")) continue
    found["C5" SUBSEP f SUBSEP p] = refq[k]
  }
  # C5 against the allow list.
  for (k in found) {
    if (k in item) { used[k] = 1; continue }
    split(k, a, SUBSEP)
    err(a[2] ":" found[k] ": " a[1] " forbids " a[3] "; fix it, or add \"" a[1] " | " a[2] " | " a[3] " | <task or none> | <reason>\" to " show_allow)
  }
  for (k in item) if (!(k in used)) {
    split(k, a, SUBSEP)
    err(show_allow ":" item[k] ": stale item " a[1] " " a[2] " " a[3] " (no such violation any more); remove it")
  }
  close("sort -u 1>&2")
  exit bad
}' $files $extra
}

self_test() {
  tmp_parent=${TMPDIR:-$repo/target}
  mkdir -p "$tmp_parent"
  tmp=$(mktemp -d "$tmp_parent/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT INT TERM
  fail=0
  expect() { # expect <want-exit> <name> <tree> [<text the output must have>]
    got=0
    (cd "$3" && check_tree list.txt allow.txt stores.rs) >"$tmp/out" 2>&1 || got=$?
    if [ "$got" -ne "$1" ] || { [ -n "${4:-}" ] && ! grep -qF -- "$4" "$tmp/out"; }; then
      echo "$me --self-test: $2: exit $got, want $1${4:+ with \"$4\"}" >&2
      sed 's/^/  /' "$tmp/out" >&2
      fail=1
    else
      echo "$me --self-test: $2: exit $got as expected"
    fi
  }
  base() { # a tree that matches its list, with no violation
    d=$tmp/$1
    mkdir -p "$d/src/application/ports" "$d/src/application/queue_reads" "$d/src/infrastructure"
    echo 'mod execution; mod planning;' >"$d/src/application/ports/mod.rs"
    cat >"$d/src/application/ports/planning.rs" <<'EOF2'
pub trait TaskStore {
    fn list(&self) -> u32;
    fn claim(&mut self);
    /// A write, internal.
    fn add(&mut self) {}
}
pub trait PlanStore {
    fn save(&mut self);
}
EOF2
    cat >"$d/src/application/ports/execution.rs" <<'EOF2'
pub trait RunReads { fn run(&self); }
pub trait RunRecovery { fn recover(&mut self); }
EOF2
    cat >"$d/src/application/inherit.rs" <<'EOF2'
pub trait InheritStore { fn inherit(&mut self); }
pub trait Helper { fn help(&self); }
pub fn carry(_: &dyn super::ports::RunRecovery, _: &dyn super::ports::TaskStore) {}
EOF2
    cat >"$d/src/application/planner.rs" <<'EOF2'
//! RunRecovery named in a comment only.
use super::ports::{RunReads, TaskStore};
use super::inherit::InheritStore;
pub fn plan(_: &dyn RunReads) { let _ = "RunRecovery in a string"; let _ = '{'; }
/* RunRecovery
   in a block comment */
#[cfg(test)]
mod tests { use super::super::ports::RunRecovery; }
EOF2
    cat >"$d/src/infrastructure/sqlite.rs" <<'EOF2'
pub fn add(conn: &Conn) {
    conn.execute("INSERT INTO tasks(id) VALUES (1)
        ON CONFLICT(id) DO UPDATE SET id = excluded.id");
}
pub fn claim_task(conn: &Conn) {
    let run = || conn.execute(r#"INSERT INTO task_runs(id) VALUES ("a")"#);
    conn.execute("INSERT INTO asks(id) VALUES (1)");
}
// UPDATE task_runs SET id = 1 in a comment does not count.
#[cfg(test)]
mod tests { fn t(c: &Conn) { c.execute("DELETE FROM run_leases"); } }
EOF2
    echo 'pub fn lease(conn: &Conn) { conn.execute("UPDATE run_leases SET run_id = 1"); }' >"$d/src/infrastructure/runs.rs"
    cat >"$d/list.txt" <<'EOF2'
# a fixture
module | src/application/planner.rs | planning |
module | src/application/inherit.rs | execution |
port | TaskStore | partial | - |
port | PlanStore | internal | - |
port | RunReads | public | all |
port | RunRecovery | internal | - |
outer-port | src/application/inherit.rs | InheritStore | execution | public | planning | fixture
not-port | src/application/inherit.rs | Helper | a fixture's helper
method | TaskStore | list | all |
method | TaskStore | claim | execution |
item | src/infrastructure/runs.rs | lease | execution | planning |
EOF2
    cat >"$d/stores.rs" <<'EOF2'
#[rustfmt::skip]
pub(super) const STORES: &[Store] = &[
    store("src/infrastructure/sqlite.rs", Planning), // a note
    store("src/infrastructure/runs.rs", Execution),
];
EOF2
    : >"$d/allow.txt"
  }

  base clean
  expect 0 "no violation (comments, strings and #[cfg(test)] are not counted)" "$tmp/clean"

  base outer_trait
  echo 'pub trait Extra { fn x(&self); }' >>"$tmp/outer_trait/src/application/planner.rs"
  expect 1 "C5 list: a pub trait outside ports/ not in the list" "$tmp/outer_trait" "src/application/planner.rs:9: C5 list: the pub trait Extra outside ports/ has no item"

  base outer_gone
  echo 'pub trait Helper { fn help(&self); }' >"$tmp/outer_gone/src/application/inherit.rs"
  expect 1 "C5 list: an outer port of the list is gone from the code" "$tmp/outer_gone" "list.txt:8: C5 list: InheritStore is no pub trait of src/application/"

  base outer_internal
  sed 's/| InheritStore | execution | public | planning |/| InheritStore | execution | internal | - |/' "$tmp/outer_internal/list.txt" >"$tmp/list" && mv "$tmp/list" "$tmp/outer_internal/list.txt"
  expect 1 "C5: another context names an internal port outside ports/" "$tmp/outer_internal" "src/application/planner.rs:3: C5 forbids InheritStore"

  base method_gone
  echo 'method | TaskStore | vanish | all |' >>"$tmp/method_gone/list.txt"
  expect 1 "C5 list: a method item names a method the partial port does not declare" "$tmp/method_gone" "list.txt:13: C5 list: the trait TaskStore declares no method vanish"

  base method_test
  echo 'method | TaskStore | gone | all |' >>"$tmp/method_test/list.txt"
  cat >>"$tmp/method_test/src/application/ports/planning.rs" <<'EOF2'
#[cfg(test)]
mod tests {
    pub trait TaskStore {
        fn gone(&self);
    }
}
EOF2
  expect 1 "C5 list: a method of a test trait of the port's name does not count" "$tmp/method_test" "list.txt:13: C5 list: the trait TaskStore declares no method gone"
  cat >"$tmp/method_test/src/application/ports/planning.rs" <<'EOF2'
pub trait TaskStore {
    fn list(&self) -> u32;
    fn claim(&mut self);
    #[cfg(test)]
    fn gone(&self);
}
pub trait PlanStore {
    fn save(&mut self);
}
mod other {
    pub trait TaskStore {
        fn gone(&self);
    }
}
EOF2
  expect 1 "C5 list: a test method of the port, or a method of another trait of its name, does not count" "$tmp/method_test" "list.txt:13: C5 list: the trait TaskStore declares no method gone"

  base partial_bare
  sed 's/^port | PlanStore | internal | - |/port | PlanStore | partial | - |/' "$tmp/partial_bare/list.txt" >"$tmp/list" && mv "$tmp/list" "$tmp/partial_bare/list.txt"
  expect 1 "C5 list: a partial port without a method item" "$tmp/partial_bare" "list.txt:5: the partial port PlanStore has no method item"

  base partial_trait
  grep -v '^method | TaskStore | list' "$tmp/partial_trait/list.txt" >"$tmp/list" && mv "$tmp/list" "$tmp/partial_trait/list.txt"
  expect 0 "C5: a partial port is public, by the trait, to the contexts of its methods" "$tmp/partial_trait"
  echo 'module | src/application/report.rs | observation |' >>"$tmp/partial_trait/list.txt"
  echo 'pub fn status(_: &dyn super::ports::TaskStore) {}' >"$tmp/partial_trait/src/application/report.rs"
  expect 1 "C5: a partial port is not public to a context none of its methods names" "$tmp/partial_trait" "src/application/report.rs:1: C5 forbids TaskStore"

  base port_unlisted
  echo 'pub trait Unlisted {}' >>"$tmp/port_unlisted/src/application/ports/planning.rs"
  expect 1 "C5 list: a port of ports/ not in the list" "$tmp/port_unlisted" "src/application/ports/planning.rs:10: C5 list: the port Unlisted has no item"

  base allow_list
  echo 'pub fn touch(_: &dyn crate::application::ports::PlanStore) {}' >>"$tmp/allow_list/src/infrastructure/runs.rs"
  expect 1 "C5: a store of another context names an internal port (the store's context from the Rust table)" "$tmp/allow_list" "src/infrastructure/runs.rs:2: C5 forbids PlanStore"
  echo 'C5 | src/infrastructure/runs.rs | PlanStore | none | fixture, unregistered' >"$tmp/allow_list/allow.txt"
  expect 0 "C5: the violation in the allow list" "$tmp/allow_list"
  echo 'C5 | src/application/planner.rs | RunRecovery | 1 | fixture' >>"$tmp/allow_list/allow.txt"
  expect 1 "a stale item of the allow list" "$tmp/allow_list" "allow.txt:2: stale item C5 src/application/planner.rs RunRecovery"
  echo 'C5 | src/infrastructure/runs.rs | PlanStore | | fixture' >"$tmp/allow_list/allow.txt"
  expect 1 "an allow item without a task" "$tmp/allow_list" "allow.txt:1: want a rule"
  echo 'C1 | src/infrastructure/runs.rs | run_leases in touch | none | fixture' >"$tmp/allow_list/allow.txt"
  expect 1 "a C1 item (the Rust test holds C1)" "$tmp/allow_list" "allow.txt:1: want a rule"

  base no_stores
  echo '// no rows' >"$tmp/no_stores/stores.rs"
  expect 1 "a Rust table with no store row" "$tmp/no_stores" "stores.rs: no store(...) row"
  base odd_store
  echo '    store(RUNS, Execution),' >>"$tmp/odd_store/stores.rs"
  expect 1 "a store row the script cannot read" "$tmp/odd_store" "stores.rs: a store row the script cannot read"

  base value_names
  echo 'module | src/application/stats.rs | observation |' >>"$tmp/value_names/list.txt"
  cat >"$tmp/value_names/src/application/stats.rs" <<'EOF2'
use crate::application::ports::execution::{RunRecovery};
fn RunRecovery() {}
fn review_probe(_: &dyn RunRecovery) {}
EOF2
  expect 1 "C5: a fn of the port's name hides no trait, and a use counts" "$tmp/value_names" "src/application/stats.rs:1: C5 forbids RunRecovery"
  cat >"$tmp/value_names/src/application/stats.rs" <<'EOF2'
fn review_probe(_: &dyn RunRecovery) {}
const RunRecovery: u32 = 0;
EOF2
  expect 1 "C5: a const of the port's name hides no trait" "$tmp/value_names" "src/application/stats.rs:1: C5 forbids RunRecovery"

  base item_gone
  sed 's/| lease | execution | planning |/| leased | execution | planning |/' "$tmp/item_gone/list.txt" >"$tmp/list" && mv "$tmp/list" "$tmp/item_gone/list.txt"
  expect 1 "C5 list: an item the code does not define" "$tmp/item_gone" "list.txt:12: C5 list: src/infrastructure/runs.rs does not define leased"

  echo '#[cfg(test)]' >"$tmp/item_gone/src/infrastructure/runs.rs"
  echo 'pub fn leased() {}' >>"$tmp/item_gone/src/infrastructure/runs.rs"
  expect 1 "C5 list: an item defined only in test code is gone" "$tmp/item_gone" "list.txt:12: C5 list: src/infrastructure/runs.rs does not define leased"

  base derived
  echo 'module | src/application/queue_reads/planning.rs | planning |' >>"$tmp/derived/list.txt"
  expect 1 "a module item for a path that tells its context" "$tmp/derived" "list.txt:13: src/application/queue_reads/planning.rs is planning by its path"

  base same_name_trait
  echo 'pub trait RunReads { fn run(&self); }' >"$tmp/same_name_trait/src/application/watch.rs"
  expect 1 "C5 list: a pub trait outside ports/ named as a listed port needs its own item" "$tmp/same_name_trait" "src/application/watch.rs:1: C5 list: the pub trait RunReads outside ports/ has no item for src/application/watch.rs"
  echo 'pub trait InheritStore { fn inherit(&mut self); }' >"$tmp/same_name_trait/src/application/watch.rs"
  expect 1 "C5 list: a second outer port of a listed name needs its own item" "$tmp/same_name_trait" "src/application/watch.rs:1: C5 list: the pub trait InheritStore outside ports/ has no item for src/application/watch.rs"

  base local_name
  echo 'module | src/application/watch.rs | observation |' >>"$tmp/local_name/list.txt"
  cat >"$tmp/local_name/src/application/watch.rs" <<'EOF2'
struct RunRecovery;
fn f(_: RunRecovery) -> self::RunRecovery { RunRecovery }
EOF2
  expect 0 "C5: a local item named as an internal port is not a reference to the port" "$tmp/local_name"
  echo 'fn g(_: &dyn crate::application::ports::RunRecovery) {}' >>"$tmp/local_name/src/application/watch.rs"
  expect 1 "C5: beside a local item of its name, a reference to the port by path counts" "$tmp/local_name" "src/application/watch.rs:3: C5 forbids RunRecovery"

  base shadow
  echo 'module | src/application/plan2.rs | planning |' >>"$tmp/shadow/list.txt"
  cat >"$tmp/shadow/src/application/plan2.rs" <<'EOF2'
use crate::application::ports::{RunRecovery};
pub fn f(_: &dyn RunRecovery) {}
EOF2
  expect 1 "C5: an internal port imported in a group counts" "$tmp/shadow" "src/application/plan2.rs:1: C5 forbids RunRecovery"
  cat >>"$tmp/shadow/src/application/plan2.rs" <<'EOF2'
#[cfg(test)]
mod tests { struct RunRecovery; }
EOF2
  expect 1 "C5: an item of the name in test code shadows nothing" "$tmp/shadow" "src/application/plan2.rs:1: C5 forbids RunRecovery"
  echo 'fn g() { struct RunRecovery; let _ = RunRecovery; }' >>"$tmp/shadow/src/application/plan2.rs"
  expect 1 "C5: an item of the name inside a fn shadows only that fn" "$tmp/shadow" "src/application/plan2.rs:1: C5 forbids RunRecovery"
  cat >"$tmp/shadow/src/application/plan2.rs" <<'EOF2'
struct RunRecovery;
fn f(_: RunRecovery) {}
mod inner {
    use crate::application::ports::{RunRecovery};
    fn h(_: &dyn RunRecovery) {}
}
EOF2
  expect 1 "C5: a mod block sees no item of the file around it" "$tmp/shadow" "src/application/plan2.rs:4: C5 forbids RunRecovery"

  base test_header
  cat >>"$tmp/test_header/src/application/planner.rs" <<'EOF2'
#[cfg(test)] use super::ports::RunRecovery;
#[cfg(test)]
impl super::ports::RunRecovery for Fake {
    fn recover(&mut self) {}
}
EOF2
  cat >>"$tmp/test_header/src/infrastructure/sqlite.rs" <<'EOF2'
#[cfg(test)]
const SEED: &str = "INSERT INTO task_runs(id) VALUES (2)";
EOF2
  echo '#[cfg(test)]' >>"$tmp/test_header/src/application/ports/execution.rs"
  echo 'pub trait FakeOnly {}' >>"$tmp/test_header/src/application/ports/execution.rs"
  expect 0 "the first line of a #[cfg(test)] item is not counted" "$tmp/test_header"

  base test_file
  echo '#[cfg(test)]' >>"$tmp/test_file/src/application/planner.rs"
  echo 'mod fakes;' >>"$tmp/test_file/src/application/planner.rs"
  mkdir -p "$tmp/test_file/src/application/planner"
  echo 'use super::super::ports::RunRecovery;' >"$tmp/test_file/src/application/planner/fakes.rs"
  echo 'module | src/application/planner/ | planning |' >>"$tmp/test_file/list.txt"
  expect 0 "a file a #[cfg(test)] mod declares is not counted" "$tmp/test_file"
  echo 'pub fn f(_: &dyn crate::application::ports::RunRecovery) {}' >>"$tmp/test_file/src/application/planner.rs"
  expect 1 "C5: another context names an internal port of ports/" "$tmp/test_file" "src/application/planner.rs:11: C5 forbids RunRecovery"

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

root=${CONTEXT_BOUNDARIES_ROOT:-$repo}
list_file=${CONTEXT_BOUNDARIES_LIST_FILE:-$repo/.config/context-boundaries.txt}
allow_file=${CONTEXT_BOUNDARIES_ALLOW_FILE:-$repo/.config/context-boundaries-allow.txt}
stores_file=${CONTEXT_BOUNDARIES_STORES_FILE:-$repo/src/context_boundaries/ownership.rs}
cd "$root"
status=0
check_tree "$list_file" "$allow_file" "$stores_file" || status=$?
[ "$status" -eq 0 ] && echo "$me: ok"
exit "$status"
