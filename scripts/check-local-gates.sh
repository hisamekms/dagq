#!/bin/sh
# Before the receipt, tell a worker which records of the local gates
# (docs/development/local-checks.md, sections "stress" and "itのtestの時間の
# 関門") the tree it is about to commit still lacks: the stress of a test the
# diff added or changed that no stress record names, a changed test file that
# names no test and has no record of a gate, and an it-test-time the diff
# requires and has no record of. The runtime judges the same from the
# receipt's commit (ADR-t2589-1 decision 6) and passes it only to the review;
# this script lets the worker see it first. It is the project's script
# (decisions 2 and 8): the runtime neither runs it nor gates on it.
#
# Usage: sh scripts/check-local-gates.sh [--base REV]
#        sh scripts/check-local-gates.sh --self-test
#   --base REV   the run's base commit; the diff starts at the merge base of
#                REV and HEAD (without it, the merge base of main and HEAD)
#   --self-test  check the script on the fixtures under
#                scripts/check-local-gates-fixtures/ in temporary git
#                repositories, with a records' directory of its own
#
# In this order:
# (0) The root is the cwd's git work tree (git rev-parse --show-toplevel),
#     and only outside any work tree the one the script is in; exit 2 when
#     that is none either.
# (1) The copy below of the declaration ($decl_tests and $decl_gates) is
#     compared with [local_checks] of the root's dagq.toml; an item that
#     differs is named and the exit is 1, before anything else and also
#     without DAGQ_LOCAL_CHECKS_DIR.
# (2) Without DAGQ_LOCAL_CHECKS_DIR (a person's shell, CI) there are no
#     records to check: it says so and exits 0.
# (3) The tree is the work tree's with every change git does not ignore,
#     made as local_check_tree of scripts/local-check-record.sh makes it, so
#     it is the tree of the records written now and of a commit of the whole
#     work tree.
# (4) The changed tests are the paths of the diff from the base to that tree
#     (git diff --no-renames) that match the tests' globs and a gate's
#     paths, counted as src/domain/local_gates.rs (changes_a_test) and the
#     Rust profile (src/domain/toolchain/rust.rs, test_lines) count them: a
#     removed path is not counted; a path under a directory named tests, one
#     named tests.rs, one that is not .rs, a change of its mode alone, and
#     one whose text or hunks cannot be read count whatever line changed,
#     and so does a .rs no [toolchains.<name>] with profile "rust" of the
#     root's dagq.toml takes by its paths; another .rs counts when its text
#     is not UTF-8 or has a NUL byte, or when a hunk of git diff -U0 meets a
#     test's item (#[test], an attribute whose path ends in test, a
#     #[cfg(...)] that names test; #![cfg(test)] puts the whole file in a
#     test), read from the tokens with comments, strings and char literals
#     left out as the runtime reads them (a multibyte character in a char
#     literal is one char; whitespace is the ASCII whitespace
#     char::is_whitespace takes; any other non-ASCII character outside
#     comments, strings and char literals makes the whole file count). A
#     path is named in git's quoted form when it has a control character,
#     ", \ or a non-ASCII byte, and is read whole however it is written.
#     Where it is unsure the script counts: it may require more than the
#     runtime does, never less. Each counted path's
#     tests are those scripts/stress-recent-tests.sh --list names (the
#     #[test] functions whose lines a hunk meets), given to the path of the
#     longest module prefix of the same binary; a counted path with none (a
#     helper's change, a file stress-recent-tests.sh does not read) is a file
#     that names no test. The tests that use a helper are not looked for.
#     --list stops before cargo builds anything, so no nextest build starts;
#     it reads a commit, so when the tree is not HEAD's the script makes an
#     unreferenced commit object of it (git commit-tree) and names that.
# (5) A gate is required when a counted path is in its paths. The records
#     read are those the review reads (domain::local_checks: the first 200
#     *.json by name, of at most 16 KiB, with trees that are hashes); those
#     that count are the ones whose tree_start and tree_end are the tree of
#     (3) and whose exit_status is 0. Each lack is one line on stderr: for
#     the gate of kind stress, each named test no such stress record's
#     command names in a filter (test(=<name>) or test(<name>), not after
#     "not "); for any gate, each file that names no test in its paths
#     while it has no such record of its kind; for another gate, being
#     required with no such record of its kind. With a lack it also says how
#     many records are of another tree (or whose tree changed while they
#     ran): run them again after the commit.
# (6) Exit 0 when nothing lacks, 1 when something does (or (1) found a
#     difference), 2 on a usage error or a git error.
#
# It writes no record, and changes neither the work tree nor the index.
# Only sh, a POSIX awk, iconv and git are needed.
set -eu

me=check-local-gates
here=$(cd "$(dirname "$0")" && pwd)

# The copy of [local_checks] of dagq.toml. When the gates or the tests of
# dagq.toml change, change this copy in the same change: (1) stops on a
# difference. A gate is "<name>|<kind>|<paths>", the globs separated by
# spaces.
decl_tests='tests/** src/** crates/**'
decl_gates='stress|stress|tests/it/** src/** crates/**
it-test-time|it-test-time|tests/it/**'

usage() {
  echo "usage: sh scripts/check-local-gates.sh [--base REV] | --self-test" >&2
  exit 2
}

base=""
self_test=0
while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage; base=$2; shift 2 ;;
    --self-test) self_test=1; shift ;;
    -h|--help) sed -n '2,/^set -eu$/p' "$0" | sed '$d'; exit 0 ;;
    *) echo "$me: unknown argument: $1" >&2; usage ;;
  esac
done

# Matches paths against globs as domain::scope::glob_matches does: * within
# one directory, ? one character, ** any number of directories.
glob_awk='
function seg_re(g,   i, c, r) {
  r = "^"
  for (i = 1; i <= length(g); i++) {
    c = substr(g, i, 1)
    if (c == "*") r = r "[^/]*"
    else if (c == "?") r = r "."
    else if (c ~ /[A-Za-z0-9_-]/) r = r c
    else if (c == "^" || c == "\\") r = r "\\" c
    else r = r "[" c "]"
  }
  return r "$"
}
function sm(gi, pi,   s) {
  if (gi > gn) return pi > pn
  if (gs[gi] == "**") {
    for (s = pi; s <= pn + 1; s++) if (sm(gi + 1, s)) return 1
    return 0
  }
  if (pi > pn) return 0
  return (ps[pi] ~ seg_re(gs[gi])) && sm(gi + 1, pi + 1)
}
function glob_match(glob, path) {
  gn = split(glob, gs, "/"); pn = split(path, ps, "/")
  return sm(1, 1)
}
function any_glob(globs, path,   n, g, i) {
  n = split(globs, g, " ")
  for (i = 1; i <= n; i++) if (glob_match(g[i], path)) return 1
  return 0
}
'

# unquote(s) is a path as git's quoted form s names it: s itself when it is
# not in quotes, else with git's C escapes (\n, \t, \", \\, \ooo) decoded.
unquote_awk='
function unquote(s,   out, i, c, n) {
  if (substr(s, 1, 1) != "\"") return s
  s = substr(s, 2, length(s) - 2); out = ""
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c != "\\") { out = out c; continue }
    c = substr(s, ++i, 1)
    if (c == "n") out = out "\n"
    else if (c == "t") out = out "\t"
    else if (c == "r") out = out "\r"
    else if (c == "a") out = out "\007"
    else if (c == "b") out = out "\010"
    else if (c == "f") out = out "\014"
    else if (c == "v") out = out "\013"
    else if (c ~ /[0-7]/) {
      n = (c + 0) * 64 + (substr(s, i + 1, 1) + 0) * 8 + (substr(s, i + 2, 1) + 0)
      i += 2; out = out sprintf("%c", n)
    } else out = out c
  }
  return out
}
'

# Reads a Rust source on stdin and prints "1" when a range of `ranges`
# ("a-b a-b ...") meets an item a test attribute is put on, as
# src/domain/toolchain/rust/test_lines.rs reads it (its doc says which), or
# when the source is unsure (below).
test_hit_awk='
BEGIN {
  nr = split(ranges, rs, " ")
  for (i = 1; i <= nr; i++) { split(rs[i], ab, "-"); lo[i] = ab[1] + 0; hi[i] = ab[2] + 0 }
  nt = 0; cdepth = 0; instr = 0
  for (i = 1; i < 256; i++) ord[sprintf("%c", i)] = i
  split("fn mod impl struct enum union static const trait type use extern unsafe async macro_rules let default", iw, " ")
  for (i in iw) itemword[iw[i]] = 1
}
function emit(t, ln) { nt++; tk[nt] = t; tl[nt] = ln }
function hashes(k,   r) { r = ""; while (k-- > 0) r = r "#"; return r }
# A word: ASCII letters, digits and _. A token is a word or one
# punctuation byte, so its first byte tells which. A non-ASCII character
# outside comments, strings and char literals is not read (the runtime
# tells Unicode whitespace, letters and punctuation apart with
# char::is_whitespace and is_alphanumeric, which awk cannot): it makes the
# file unsure, and an unsure file counts whatever line changed.
function isword(t) { return t != "" && wordch(substr(t, 1, 1)) }
function wordch(c) { return c ~ /[A-Za-z0-9_]/ }
function lex(src, ln,   i, n, ch, nx, k, h, p, start, o, w) {
  n = length(src); i = 1
  while (i <= n) {
    ch = substr(src, i, 1); nx = substr(src, i + 1, 1)
    if (cdepth > 0) {
      if (ch == "/" && nx == "*") { cdepth++; i += 2 }
      else if (ch == "*" && nx == "/") { cdepth--; i += 2 }
      else i++
      continue
    }
    if (instr == 1) {
      if (ch == "\\") i += 2
      else { if (ch == "\"") instr = 0; i++ }
      continue
    }
    if (instr == 2) {
      if (ch == "\"" && substr(src, i + 1, rawhash) == hashes(rawhash)) { instr = 0; i += 1 + rawhash }
      else i++
      continue
    }
    if (ord[ch] >= 128) { unsure = 1; exit }
    # The ASCII characters char::is_whitespace takes: space, tab, CR, VT, FF
    # (a newline ends the line).
    if (ch == " " || ch == "\t" || ch == "\r" || ord[ch] == 11 || ord[ch] == 12) { i++; continue }
    if (ch == "/" && nx == "/") break
    if (ch == "/" && nx == "*") { cdepth = 1; i += 2; continue }
    if (ch == "\"") { instr = 1; i++; continue }
    if (ch == "'\''") {
      if (nx == "\\") {
        i += 3
        while (i <= n && substr(src, i, 1) != "'\''") i++
        i++
      } else {
        # The script reads bytes and the runtime chars: a char literal of
        # one multibyte character is its bytes then the quote.
        o = ord[nx]; w = o >= 240 ? 4 : o >= 224 ? 3 : o >= 192 ? 2 : 1
        if (substr(src, i + 1 + w, 1) == "'\''") i += 2 + w
        else i++
      }
      continue
    }
    if (wordch(ch)) {
      p = i
      if (ch == "b" || ch == "c") p = i + 1
      if (p > i && substr(src, p, 1) == "\"") { instr = 1; i = p + 1; continue }
      if (ch == "b" && nx == "'\''") { i++; continue }
      if (substr(src, p, 1) == "r") {
        k = p + 1; h = 0
        while (substr(src, k, 1) == "#") { h++; k++ }
        if (substr(src, k, 1) == "\"") { instr = 2; rawhash = h; i = k + 1; continue }
      }
      start = i
      while (i <= n && wordch(substr(src, i, 1))) i++
      emit(substr(src, start, i - start), ln)
      continue
    }
    emit(ch, ln); i++
  }
}
{ lex($0, NR) }
function closing(i, op, cl,   d) {
  d = 0
  for (; i <= nt; i++) {
    if (tk[i] == op) d++
    else if (tk[i] == cl) { d--; if (d == 0) return i }
  }
  return 0
}
function is_test_attr(a, b,   i, pe) {
  if (tk[a] == "cfg") {
    for (i = a + 1; i <= b; i++) if (tk[i] == "test") return 1
    return 0
  }
  pe = b + 1
  for (i = a; i <= b; i++) if (!(isword(tk[i]) || tk[i] == ":")) { pe = i; break }
  return pe > a && tk[pe - 1] == "test"
}
function is_item(i,   op, c) {
  while (1) {
    if (tk[i] == "#") {
      op = i + 1 + (tk[i + 1] == "!")
      if (tk[op] != "[") return 0
      c = closing(op, "[", "]")
      if (!c) return 0
      i = c + 1
      continue
    }
    if (tk[i] == "pub") {
      i++
      if (tk[i] == "(") { c = closing(i, "(", ")"); if (!c) return 0; i = c + 1 }
      continue
    }
    if (isword(tk[i])) return (tk[i] in itemword)
    return 0
  }
}
function item_end(idx,   commas, depth, angles, at, t, after_arrow, mc, j, c) {
  commas = !is_item(idx); depth = 0; angles = 0
  for (at = idx; at <= nt; at++) {
    t = tk[at]
    after_arrow = (at > 1 && (tk[at - 1] == "-" || tk[at - 1] == "="))
    mc = 0
    if (at > idx && tk[at - 1] == "!") {
      for (j = idx; j < at - 1; j++) if (!(isword(tk[j]) || tk[j] == ":")) { mc = 1; break }
    }
    if (t == "<") angles++
    else if (t == ">" && angles > 0 && !after_arrow) angles--
    else if (t == "(" || t == "[") depth++
    else if (t == "{" && (depth > 0 || angles > 0 || mc)) depth++
    else if ((t == ")" || t == "]" || t == "}") && depth > 0) depth--
    else if (t == "{") { c = closing(at, "{", "}"); return c ? c : nt }
    else if (t == ";" && depth == 0) return at
    else if (t == "," && commas && depth == 0 && angles == 0) return at
    else if (t == "}" || t == ")" || t == "]") return at - 1
  }
  return nt
}
function hit(a, b,   i) {
  for (i = 1; i <= nr; i++) if (lo[i] <= b && a <= hi[i]) return 1
  return 0
}
END {
  if (unsure) { print 1; exit }
  for (t = 1; t <= nt; t++) {
    if (tk[t] != "#") continue
    inner = (tk[t + 1] == "!"); op = t + 1 + inner
    if (tk[op] != "[") continue
    cl = closing(op, "[", "]")
    if (!cl || !is_test_attr(op + 1, cl - 1)) continue
    if (inner) { if (nr > 0) print 1; exit }
    e = item_end(cl + 1)
    if (hit(tl[t], tl[e])) { print 1; exit }
  }
}
'

# The stress-recent-tests.sh target of a path: "<binary id>|<module prefix>",
# or nothing for a path it does not read.
target_of() {
  case "$1" in
    tests/e2e.rs|tests/e2e/*|tests/plugin.rs) return 0 ;;
    tests/it/main.rs) echo "dagq::it|" ;;
    tests/it/*.rs)
      m=${1#tests/it/}; m=${m%.rs}; m=${m%/mod}
      echo "dagq::it|$(echo "$m" | sed 's#/#::#g')::" ;;
    src/main.rs) echo "dagq::bin/dagq|" ;;
    src/lib.rs) echo "dagq|" ;;
    src/*.rs)
      m=${1#src/}; m=${m%.rs}; m=${m%/mod}
      echo "dagq|$(echo "$m" | sed 's#/#::#g')::" ;;
  esac
}

# The declaration of the root's dagq.toml as lines "tests <globs>",
# "gate <name> kind <kind>", "gate <name> paths <globs>", "toolchain <name>
# profile <profile>" and "toolchain <name> paths <globs>", the globs
# separated by one space; arrays may span lines.
declared_in() {
  awk '
function strip(s,   out, i, c, q) {
  out = ""; q = 0
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == "\"") q = !q
    if (c == "#" && !q) break
    out = out c
  }
  return out
}
function words(v,   out, n, a, i) {
  gsub(/[][,]/, " ", v); n = split(v, a, " "); out = ""
  for (i = 1; i <= n; i++) { gsub(/"/, "", a[i]); out = out (out == "" ? "" : " ") a[i] }
  return out
}
{
  line = strip($0)
  if (pending != "") {
    value = value " " line
    if (line ~ /\]/) { emitv(pending, value); pending = "" }
    next
  }
  if (line ~ /^[ \t]*\[/) {
    sec = line; gsub(/[ \t]/, "", sec); gsub(/^\[+|\]+$/, "", sec)
    next
  }
  if (line !~ /=/) next
  key = line; sub(/=.*/, "", key); gsub(/[ \t]/, "", key)
  value = line; sub(/^[^=]*=/, "", value)
  if (sec == "local_checks" && key == "tests") name = "tests"
  else if (sec ~ /^local_checks\.gates\./ && (key == "paths" || key == "kind")) {
    g = sec; sub(/^local_checks\.gates\./, "", g); gsub(/"/, "", g)
    name = "gate " g " " key
  } else if (sec ~ /^toolchains\./ && (key == "profile" || key == "paths")) {
    g = sec; sub(/^toolchains\./, "", g); gsub(/"/, "", g)
    name = "toolchain " g " " key
  } else next
  if (value ~ /\[/ && value !~ /\]/) { pending = name; next }
  emitv(name, value)
}
function emitv(name, v) { print name " " words(v) }
' "$1" | sort
}

declared_copy() {
  {
    echo "tests $decl_tests"
    echo "$decl_gates" | while IFS='|' read -r g k p; do
      echo "gate $g kind $k"
      echo "gate $g paths $p"
    done
  } | sort
}

# (1) on the root $1: prints each item that differs and fails when one does.
check_declaration() {
  if [ ! -f "$1/dagq.toml" ]; then
    echo "$me: $1/dagq.toml が無いので [local_checks] を script の写しと比べられない" >&2
    return 1
  fi
  declared_copy > "$tmp/decl-copy"
  declared_in "$1/dagq.toml" | grep -E '^(tests|gate) ' > "$tmp/decl-toml" || true
  if cmp -s "$tmp/decl-copy" "$tmp/decl-toml"; then return 0; fi
  awk '
    FNR == NR { k = $1 == "tests" ? "tests" : $1 " " $2 " " $3; c[k] = $0; next }
    { k = $1 == "tests" ? "tests" : $1 " " $2 " " $3; t[k] = $0 }
    END {
      for (k in c) if (c[k] != t[k]) printf "%s\t%s\t%s\n", k, c[k], (k in t ? t[k] : "(無い)")
      for (k in t) if (!(k in c)) printf "%s\t(無い)\t%s\n", k, t[k]
    }' "$tmp/decl-copy" "$tmp/decl-toml" | sort |
  while IFS='	' read -r item copy toml; do
    echo "$me: dagq.toml の [local_checks] が script の写しと違う: $item（dagq.toml: $toml、script: $copy）。同じ変更で scripts/check-local-gates.sh の写しを直す" >&2
  done
  return 1
}

# Every record of $1 as "<kind>\t<exit status>\t<1 when of tree $2>\t<command>",
# of those the review reads (review::read_local_checks, by
# domain::local_checks::records_to_read): the first MAX_RECORDS *.json in the
# order of their names' bytes, each of at most RECORD_BYTES, its trees hashes.
records_of() {
  ls "$1" 2>/dev/null | grep '\.json$' | LC_ALL=C sort | head -n 200 | while IFS= read -r f; do
    [ -f "$1/$f" ] || continue
    [ "$(wc -c < "$1/$f" | tr -d ' ')" -le 16384 ] || continue
    tr -d '\n' < "$1/$f"; echo
  done | awk -v tree="$2" '
    function hash(h) { return (length(h) == 40 || length(h) == 64) && h !~ /[^0-9a-fA-F]/ }
    /"exit_status":/ {
      cmd = $0; sub(/^\{"command":"/, "", cmd); i = index(cmd, "\",\"exit_status\":"); if (i) cmd = substr(cmd, 1, i - 1)
      st = $0; sub(/.*"exit_status":/, "", st); sub(/[^0-9-].*/, "", st)
      kd = $0; if (kd ~ /"kind":"/) { sub(/.*"kind":"/, "", kd); sub(/".*/, "", kd) } else kd = ""
      ts = $0; sub(/.*"tree_start":"/, "", ts); sub(/".*/, "", ts)
      te = $0; sub(/.*"tree_end":"/, "", te); sub(/".*/, "", te)
      if (!hash(ts) || !hash(te)) next
      printf "%s\t%s\t%d\t%s\n", kd, st, (ts == tree && te == tree), cmd
    }'
}

main() {
  # (0)
  root=$(git rev-parse --show-toplevel 2>/dev/null) ||
    root=$(git -C "$here/.." rev-parse --show-toplevel 2>/dev/null) || {
      echo "$me: neither the cwd nor $here is in a git work tree" >&2
      exit 2
    }
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT

  # (1)
  check_declaration "$root" || exit 1

  # (2)
  if [ -z "${DAGQ_LOCAL_CHECKS_DIR:-}" ]; then
    echo "$me: DAGQ_LOCAL_CHECKS_DIR が無いので記録を確かめられない（人の手元・CI）。[local_checks] と写しの照合だけ通った"
    exit 0
  fi
  dir=$DAGQ_LOCAL_CHECKS_DIR
  cd "$root"

  # (3)
  record_lib=$here/local-check-record.sh
  [ -f "$record_lib" ] || record_lib=$root/scripts/local-check-record.sh
  # shellcheck source=scripts/local-check-record.sh
  . "$record_lib"
  tree=$(local_check_tree) || { echo "$me: cannot read the tree of the work tree" >&2; exit 2; }

  head=$(git rev-parse --verify --quiet 'HEAD^{commit}') || { echo "$me: no HEAD" >&2; exit 2; }
  if [ -n "$base" ]; then
    base_sha=$(git rev-parse --verify --quiet "$base^{commit}") || { echo "$me: no such commit: $base" >&2; exit 2; }
  else
    base_sha=$(git rev-parse --verify --quiet 'main^{commit}') || { echo "$me: no main to take the merge base with; give --base" >&2; exit 2; }
    echo "$me: --base が無いので main との merge base から数える。main を run の branch に merge したなら、runtime と同じ範囲にするため run の base commit を --base に渡す" >&2
  fi
  from=$(git merge-base "$base_sha" "$head" 2>/dev/null) || from=$base_sha

  # (4) The candidates: "<quoted path>\t<gate names>\t<1 when the Rust
  # profile reads it>". A path is kept in
  # git's quoted form (core.quotePath: a path with a control character, ",
  # \ or a byte over 127 is in quotes with C escapes), one per line with no
  # tab or newline in it; the globs and git read it decoded.
  git -c core.quotePath=true diff --name-only --no-renames --no-ext-diff --no-textconv "$from" "$tree" -- > "$tmp/changed" ||
    { echo "$me: cannot diff $from with the tree" >&2; exit 2; }
  # The globs of the paths the Rust profile tells the lines of, as
  # Toolchains::test_lines takes the declarations of [toolchains.<name>]
  # with profile "rust": their paths, or every path for one without. With
  # none declared, no .rs path is read by lines: each counts whole.
  rust_globs=$(declared_in "$root/dagq.toml" | awk '
    $1 == "toolchain" && $3 == "profile" && $4 == "rust" { rust[$2] = 1 }
    $1 == "toolchain" && $3 == "paths" { n = $2; $1 = $2 = $3 = ""; sub(/^ +/, ""); paths[n] = $0 }
    END { for (n in rust) printf "%s ", (n in paths && paths[n] != "" ? paths[n] : "**") }')
  # awk takes no newline in -v: the gates are separated by ";" there.
  gates_v=$(echo "$decl_gates" | tr '\n' ';')
  LC_ALL=C awk -v tests="$decl_tests" -v gates="$gates_v" -v rust="$rust_globs" "$glob_awk$unquote_awk"'
    BEGIN { ng = split(gates, gl, ";"); for (i = 1; i <= ng; i++) { if (gl[i] == "") continue; split(gl[i], f, "|"); gname[i] = f[1]; gpaths[i] = f[3] } }
    {
      path = unquote($0)
      if (!any_glob(tests, path)) next
      in_gates = ""
      for (i = 1; i <= ng; i++) if (any_glob(gpaths[i], path)) in_gates = in_gates " " gname[i]
      if (in_gates != "") print $0 "\t" substr(in_gates, 2) "\t" (rust != "" && any_glob(rust, path))
    }' "$tmp/changed" > "$tmp/candidates"

  : > "$tmp/counted"
  while IFS='	' read -r qpath gates by_rust; do
    case "$qpath" in
      '"'*) path=$(printf '%s\n' "$qpath" | LC_ALL=C awk "$unquote_awk"'{ printf "%s", unquote($0) }'; printf x); path=${path%x} ;;
      *) path=$qpath ;;
    esac
    # Removed when the tree has no entry of it; an entry whose text cannot
    # be read (a gitlink, a text that is not UTF-8) counts below.
    [ -n "$(git --literal-pathspecs ls-tree "$tree" -- "$path" 2>/dev/null)" ] || continue
    counted=1
    file=${path##*/}
    case "/$path" in
      */tests/*) ;;
      *)
        case "$file" in
          tests.rs) ;;
          *.rs)
            # A NUL byte would cut awk's line short: such a text counts.
            if [ "$by_rust" = 1 ] &&
              git cat-file blob "$tree:$path" > "$tmp/text" 2>/dev/null &&
              iconv -f UTF-8 -t UTF-8 "$tmp/text" >/dev/null 2>&1 &&
              [ "$(tr -d '\000' < "$tmp/text" | wc -c)" -eq "$(wc -c < "$tmp/text")" ] &&
              git --literal-pathspecs diff -U0 --no-color --no-renames --no-ext-diff --no-textconv "$from" "$tree" -- "$path" > "$tmp/diff" 2>/dev/null; then
              ranges=$(sed -n 's/^@@ -[^ ]* +\([0-9][0-9]*\)\(,\([0-9][0-9]*\)\)\{0,1\} @@.*/\1 \3/p' "$tmp/diff" |
                awk '{ c = ($2 == "") ? 1 : $2 + 0
                       if (c == 0) printf "%d-%d ", ($1 < 1 ? 1 : $1), $1 + 1
                       else printf "%d-%d ", $1, $1 + c - 1 }')
              if [ -n "$ranges" ]; then
                hit=$(LC_ALL=C awk -v ranges="$ranges" "$test_hit_awk" < "$tmp/text")
                [ "$hit" = 1 ] || counted=0
              fi
            fi ;;
        esac ;;
    esac
    [ "$counted" = 1 ] || continue
    # A quoted path names no module: it is a file that names no test.
    printf '%s\t%s\t%s\n' "$qpath" "$gates" "$(target_of "$qpath")" >> "$tmp/counted"
  done < "$tmp/candidates"

  if [ ! -s "$tmp/counted" ]; then
    echo "$me: $from からの差分に変えた test は無いので、stress と it の時間の関門は要らない"
    exit 0
  fi

  # The named tests of the tree, by stress-recent-tests.sh --list.
  if [ "$(git rev-parse 'HEAD^{tree}')" = "$tree" ]; then
    commit=$head
  else
    commit=$(GIT_AUTHOR_NAME=$me GIT_AUTHOR_EMAIL=$me@localhost GIT_COMMITTER_NAME=$me GIT_COMMITTER_EMAIL=$me@localhost \
      git commit-tree "$tree" -p "$head" -m "$me: the work tree" </dev/null) ||
      { echo "$me: cannot make a commit object of the tree" >&2; exit 2; }
  fi
  stress_script=$here/stress-recent-tests.sh
  [ -f "$stress_script" ] || stress_script=$root/scripts/stress-recent-tests.sh
  sh "$stress_script" --list --base "$from" --head "$commit" > "$tmp/stress-list" 2>&1 || {
    echo "$me: $stress_script --list failed:" >&2
    sed 's/^/  /' "$tmp/stress-list" >&2
    exit 2
  }
  sed -n 's/^  \([^ ][^ ]*\) \([^ ][^ ]*\)$/\1 \2/p' "$tmp/stress-list" > "$tmp/names"

  records_of "$dir" "$tree" > "$tmp/records"

  # (5)
  awk -v gates="$gates_v" -v me="$me" '
    BEGIN { FS = "\t" }
    FILENAME == ARGV[1] {
      np++; cpath[np] = $1; cgates[np] = " " $2 " "; split($3, tg, "|"); cbin[np] = $3 == "" ? "" : tg[1]; cpre[np] = tg[2]; ctarget[np] = $3
      next
    }
    FILENAME == ARGV[2] {
      split($0, w, " "); nn++; nbin[nn] = w[1]; nname[nn] = w[2]
      next
    }
    {
      if ($3 != 1) { other[$1]++; next }
      if ($2 != 0) { failed[$1]++; next }
      good[$1]++; ngood[$1]++; gcmd[$1, ngood[$1]] = $4
    }
    # A command names a test by test(=NAME) (or test(NAME)) of a filter
    # expression, with no "not " right before it.
    function names_in(cmd, name,   s, i, k, pre) {
      for (k = 1; k <= 2; k++) {
        pre = k == 1 ? "test(=" : "test("
        s = cmd
        while ((i = index(s, pre name ")")) > 0) {
          if (i < 5 || substr(s, i - 4, 4) != "not ") return 1
          s = substr(s, i + 1)
        }
      }
      return 0
    }
    END {
      # Each name to the counted path of its binary with the longest prefix.
      for (n = 1; n <= nn; n++) {
        best = 0
        for (p = 1; p <= np; p++)
          if (ctarget[p] != "" && cbin[p] == nbin[n] && index(nname[n], cpre[p]) == 1 && (best == 0 || length(cpre[p]) > length(cpre[best]))) best = p
        owner[n] = best; if (best) named[best] = 1
      }
      ng = split(gates, gl, ";"); lack = 0
      for (g = 1; g <= ng; g++) {
        if (gl[g] == "") continue
        split(gl[g], f, "|"); name = f[1]; kind = f[2]; req = ""
        for (p = 1; p <= np; p++) if (index(cgates[p], " " name " ")) req = req " " cpath[p]
        if (req == "") continue
        if (kind == "stress") {
          for (n = 1; n <= nn; n++) {
            if (owner[n] && !index(cgates[owner[n]], " " name " ")) continue
            ok = 0
            for (r = 1; r <= ngood[kind] && !ok; r++) ok = names_in(gcmd[kind, r], nname[n])
            if (!ok) { lack++; printf "%s: %s: %s を名指す %s（kind %s）の exit 0 の記録が最終 tree に無い\n", me, (owner[n] ? cpath[owner[n]] : "?"), nname[n], name, kind > "/dev/stderr" }
          }
        }
        for (p = 1; p <= np; p++) {
          if (!index(cgates[p], " " name " ") || named[p] || good[kind]) continue
          lack++; printf "%s: %s: test を名指せない変更なので %s（kind %s）の記録が最終 tree で 1 件以上要る\n", me, cpath[p], name, kind > "/dev/stderr"
        }
        if (kind != "stress" && !good[kind]) {
          lack++; printf "%s: %s（kind %s）: 変えた test（%s）があるのに、最終 tree で exit 0 の記録が無い\n", me, name, kind, substr(req, 2) > "/dev/stderr"
        }
      }
      if (!lack) exit 0
      for (k in other) o += other[k]
      for (k in failed) x += failed[k]
      if (o) printf "%s: 最終 tree と一致しない記録（tree が違うか、実行の間に tree が変わった）が %d 件ある。commit の後に流し直す\n", me, o > "/dev/stderr"
      if (x) printf "%s: 最終 tree の記録のうち exit 0 でないものが %d 件ある\n", me, x > "/dev/stderr"
      exit 1
    }' "$tmp/counted" "$tmp/names" "$tmp/records" || {
    echo "$me: 名指した stress・関門を最終 tree で流す（test を名指せない file は、その file を使う変えた test か、その gate の対象の test を選ぶ）か、流せない理由を receipt の evidence に書く（docs/development/local-checks.md）" >&2
    exit 1
  }
  echo "$me: 最終 tree（$tree）に、差分が要る stress と it の時間の関門の記録がそろっている"
}

# self_test builds small git histories from the fixtures and checks each
# case's exit status and what it names.
self_test() {
  fixtures=$here/check-local-gates-fixtures
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT
  tmp=$(cd "$tmp" && pwd)
  unset DAGQ_LOCAL_CHECKS_DIR
  script=$here/check-local-gates.sh
  . "$here/local-check-record.sh"
  fail=0
  repo=$tmp/repo
  recs=$tmp/records
  g() { git -C "$repo" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }

  # fresh: a repository with the base committed on main and a branch run.
  fresh() {
    rm -rf "$repo" "$recs"; mkdir -p "$repo" "$recs"
    g init -q -b main
    cp -R "$fixtures/base/." "$repo/"
    g add -A && g commit -q -m base
    g checkout -q -b run
  }
  # put <file under changes/> <path>: the changed file in the work tree.
  put() { cp "$fixtures/changes/$1" "$repo/$2"; }
  commit() { g add -A && g commit -q -m change; }
  tree() { (cd "$repo" && local_check_tree); }
  # record <kind or ""> <exit status> <tree> <command>
  record() {
    n=$(ls "$recs" | wc -l | tr -d ' ')
    if [ -n "$1" ]; then k="\"$1\""; else k=null; fi
    printf '{"command":"%s","exit_status":%d,"started_at":"2026-10-24T00:00:00Z","duration_ms":1,"kind":%s,"tree_start":"%s","tree_end":"%s","log":"r%s.log"}\n' \
      "$4" "$2" "$k" "$3" "$3" "$n" > "$recs/r$n.json"
  }
  # state: what the script must not change.
  state() { (cd "$repo" && git status --porcelain=v1 -uall && git ls-files -s && ls "$recs") | cksum; }
  # check <case> <want exit> [<text the output has>...]; "!text" for one it
  # must not have.
  check() {
    c=$1 want=$2; shift 2
    before=$(state)
    got=0
    (cd "$repo" && DAGQ_LOCAL_CHECKS_DIR=${use_dir-$recs} sh "$script" ${base_arg:-}) > "$tmp/out" 2>&1 || got=$?
    ok=1
    [ "$got" -eq "$want" ] || ok=0
    [ "$(state)" = "$before" ] || { echo "$me --self-test: $c: changed the work tree, the index or the records" >&2; ok=0; }
    for t in "$@"; do
      case "$t" in
        !*) if grep -qF -- "${t#!}" "$tmp/out"; then ok=0; fi ;;
        *) grep -qF -- "$t" "$tmp/out" || ok=0 ;;
      esac
    done
    if [ "$ok" = 1 ]; then
      echo "$me --self-test: $c: ok"
    else
      echo "$me --self-test: $c: FAILED (exit $got, want $want; want $*)" >&2
      sed 's/^/  /' "$tmp/out" >&2
      fail=1
    fi
  }
  stress_both="cargo nextest run --locked --test it --stress-count 5 -E 'test(=alpha::a_one) | test(=alpha::a_two)'"

  # Both tests of tests/it/alpha.rs changed and committed.
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  record stress 0 "$t" "$stress_both"; record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh --base main it.log"
  check every_record_there 0
  base_arg="--base main"; check every_record_there_with_base 0; base_arg=""
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  record stress 0 "$t" "cargo nextest run --locked --test it --stress-count 5 -E 'test(=alpha::a_one)'"
  record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh --base main it.log"
  check stress_names_part 1 "alpha::a_two を名指す stress" "!alpha::a_one を名指す"
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit
  record stress 0 0123456789abcdef0123456789abcdef01234567 "$stress_both"
  record it-test-time 0 0123456789abcdef0123456789abcdef01234567 "sh scripts/check-it-test-time.sh"
  check other_tree 1 "alpha::a_one を名指す stress" "it-test-time（kind it-test-time）" "2 件ある。commit の後に流し直す"
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  record stress 1 "$t" "$stress_both"; record it-test-time 1 "$t" "sh scripts/check-it-test-time.sh"
  check failed_records 1 "alpha::a_two を名指す stress" "it-test-time（kind it-test-time）" "exit 0 でないものが 2 件"
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  record stress 0 "$t" "$stress_both"
  check no_it_test_time 1 "it-test-time（kind it-test-time）: 変えた test（tests/it/alpha.rs）" "!を名指す stress"
  # Uncommitted: the records of the work tree's tree count.
  fresh; put alpha_tests.rs tests/it/alpha.rs; t=$(tree)
  record stress 0 "$t" "$stress_both"; record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh"
  check uncommitted_change 0

  # A helper of tests/it alone: a file with no #[test], and a line outside
  # the #[test] of a test file.
  fresh; put support.rs tests/it/support.rs; commit
  check helper_file_no_record 1 "tests/it/support.rs: test を名指せない変更なので stress（kind stress）" "tests/it/support.rs: test を名指せない変更なので it-test-time（kind it-test-time）"
  t=$(tree); record stress 0 "$t" "cargo nextest run --locked --test it --stress-count 5 -E 'test(=alpha::a_one)'"
  record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh"
  check helper_file_with_records 0
  fresh; put alpha_helper.rs tests/it/alpha.rs; commit
  check helper_lines_no_record 1 "tests/it/alpha.rs: test を名指せない変更なので stress" "tests/it/alpha.rs: test を名指せない変更なので it-test-time"
  t=$(tree); record stress 0 "$t" "cargo nextest run --locked --test it -E 'test(=alpha::a_one)'"
  record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh"
  check helper_lines_with_records 0

  # src: a line of the #[cfg(test)] module that is in no #[test], and a
  # production line alone.
  fresh; put lib_test_helper.rs src/lib.rs; commit
  check src_test_module_helper 1 "src/lib.rs: test を名指せない変更なので stress（kind stress）" "!it-test-time"
  fresh; put lib_production.rs src/lib.rs; commit
  check src_production_only 0 "変えた test は無い"
  fresh; put lib_test.rs src/lib.rs; commit
  check src_test_changed 1 "src/lib.rs: tests::checks_answer を名指す stress"

  # A char literal of one multibyte character does not end the test early.
  fresh; put chars_test.rs src/chars.rs; commit
  check multibyte_char_literal 1 "src/chars.rs: "
  # A filter that leaves a test out does not name it.
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  record stress 0 "$t" "cargo nextest run --locked --test it -E 'test(=alpha::a_two) | not test(=alpha::a_one)'"
  record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh"
  check not_in_filter 1 "alpha::a_one を名指す stress" "!alpha::a_two を名指す"

  # An attribute whose path has a word of non-ASCII letters and ends in test.
  fresh; put unicode_attr.rs src/unicode_attr.rs; commit
  check unicode_attribute_path 1 "src/unicode_attr.rs: test を名指せない変更なので stress"
  # Whitespace the runtime skips (char::is_whitespace) inside an attribute:
  # a form feed and a vertical tab.
  fresh; printf '#[\ftest]\nfn under_a_form_feed() {\n    let x = 1;\n}\n' > "$repo/src/form_feed.rs"
  printf '#[cfg(\vtest)]\nfn under_a_vertical_tab() {\n    let x = 1;\n}\n' > "$repo/src/vertical_tab.rs"; commit
  sed 's/let x = 1;/let x = 2;/' "$repo/src/form_feed.rs" > "$tmp/ff" && cp "$tmp/ff" "$repo/src/form_feed.rs"
  sed 's/let x = 1;/let x = 2;/' "$repo/src/vertical_tab.rs" > "$tmp/vt" && cp "$tmp/vt" "$repo/src/vertical_tab.rs"
  base_arg="--base HEAD"
  check form_feed_and_vertical_tab 1 "src/form_feed.rs: test を名指せない変更なので stress" "src/vertical_tab.rs: test を名指せない変更なので stress"
  # A non-ASCII character in code (here a no-break space the runtime takes
  # as whitespace) makes the whole file count, also for a production line.
  fresh; printf 'pub fn production() -> u32 {\n    1\n}\n\n#[cfg(\302\240test)]\nmod tests {}\n' > "$repo/src/nbsp.rs"; commit
  sed 's/^    1$/    2/' "$repo/src/nbsp.rs" > "$tmp/nb" && cp "$tmp/nb" "$repo/src/nbsp.rs"
  check non_ascii_in_code 1 "src/nbsp.rs: test を名指せない変更なので stress"
  # The same production line of an ASCII file is no test's.
  fresh; printf 'pub fn production() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {}\n' > "$repo/src/ascii.rs"; commit
  sed 's/^    1$/    2/' "$repo/src/ascii.rs" > "$tmp/as" && cp "$tmp/as" "$repo/src/ascii.rs"
  check production_line_of_an_ascii_file 0 "変えた test は無い"
  base_arg=""
  # Without the Rust profile's declaration no .rs is read by lines.
  fresh; awk '/^\[toolchains\.rust\]$/ { skip = 1; next } /^\[/ { skip = 0 } !skip' "$repo/dagq.toml" > "$tmp/toml" && cp "$tmp/toml" "$repo/dagq.toml"
  g add -A && g commit -q -m 'no rust profile' && g branch -f main HEAD
  put lib_production.rs src/lib.rs; commit
  check no_rust_profile 1 "src/lib.rs: test を名指せない変更なので stress"

  # A path with a newline or a tab is one path, not two.
  fresh; nl_path=$(printf 'tests/it/a\nb.rs'); tab_path=$(printf 'tests/it/c\td.rs')
  echo '#[test] fn t() {}' > "$repo/$nl_path"; commit
  check newline_in_a_path 1 '"tests/it/a\nb.rs": test を名指せない変更なので stress' "!変えた test は無い"
  fresh; echo '#[test] fn t() {}' > "$repo/$tab_path"; commit
  check tab_in_a_path 1 '"tests/it/c\td.rs": test を名指せない変更なので it-test-time'
  # Over the review's limit of records: it reads the first 200 by name, so
  # the matching records after them do not count.
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit; t=$(tree)
  i=0
  while [ "$i" -lt 200 ]; do
    printf '{"command":"old","exit_status":0,"started_at":"2026-10-24T00:00:00Z","duration_ms":1,"kind":"stress","tree_start":"%s","tree_end":"%s","log":"a.log"}\n' \
      0123456789abcdef0123456789abcdef01234567 0123456789abcdef0123456789abcdef01234567 > "$recs/a-$(printf %03d "$i").json"
    i=$((i + 1))
  done
  record stress 0 "$t" "$stress_both"; record it-test-time 0 "$t" "sh scripts/check-it-test-time.sh"
  check over_the_records_limit 1 "alpha::a_one を名指す stress" "it-test-time（kind it-test-time）" "200 件ある"

  # A removed test file alone.
  fresh; g rm -q tests/it/alpha.rs; commit
  check removed_test_file 0

  # Without DAGQ_LOCAL_CHECKS_DIR.
  fresh; put alpha_tests.rs tests/it/alpha.rs; commit
  use_dir=""; check no_records_dir 0 "DAGQ_LOCAL_CHECKS_DIR が無い"; unset use_dir

  # The declaration differs from the copy, also without the records.
  fresh; sed 's/^kind = "it-test-time"$/kind = "it-time"/' "$repo/dagq.toml" > "$tmp/toml" && cp "$tmp/toml" "$repo/dagq.toml"
  check declaration_drift 1 "gate it-test-time kind" "script の写しと違う"
  use_dir=""
  check declaration_drift_without_records 1 "gate it-test-time kind"; unset use_dir
  fresh; sed 's#^tests = .*#tests = ["tests/**", "src/**"]#' "$repo/dagq.toml" > "$tmp/toml" && cp "$tmp/toml" "$repo/dagq.toml"
  check declaration_drift_tests 1 "dagq.toml の [local_checks] が script の写しと違う: tests"

  # The declaration of this repository's dagq.toml is the copy's.
  if [ -f "$here/../dagq.toml" ]; then
    rm -rf "$repo"; mkdir -p "$repo"; cp "$here/../dagq.toml" "$repo/dagq.toml"
    if check_declaration "$repo"; then
      echo "$me --self-test: this repository's dagq.toml matches the copy: ok"
    else
      echo "$me --self-test: this repository's dagq.toml differs from the copy: FAILED" >&2; fail=1
    fi
  fi

  if [ "$fail" -ne 0 ]; then exit 1; fi
  echo "$me --self-test: ok"
}

if [ "$self_test" = 1 ]; then
  self_test
else
  main
fi
