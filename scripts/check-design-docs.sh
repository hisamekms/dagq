#!/bin/sh
# Check the budgets of the documents under docs/design (ADR-t1942-1): the size
# of each document, the length of each line and the task numbers in the body.
# The budgets, which documents are concept documents, and the boundaries of
# what is measured (frontmatter, fenced code blocks, inline code, link
# targets, the count words) are in docs/development/documents.md, section
# "design" ("形と予算" and "検査の境界"); this script only applies them.
# The dates in the frontmatter are checked by scripts/check-frontmatter-dates.sh.
#
# A document over a budget today is named in the allow list
# (.config/design-docs-allow.toml) with the value it has, which is then its
# limit, so it may shrink but not grow. Two checks:
#
# (i)  the tree: a document over a budget fails unless its item allows the
#      value; an item of a document that does not exist, and an item's value
#      for a measure the document is within the budget of, fail too (remove
#      them in the change that brings the document within the budget);
# (ii) the allow list against the base: an item or a value the base's list
#      has not, and a value larger than the base's, fail (lowering a value or
#      removing an item passes). Skipped when the base has no allow list.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"); the allow list and the base are read from it.
# Outside a git work tree it is the repository found from the script's own
# location. Only sh, a POSIX awk and git are needed.
#
# Usage: sh scripts/check-design-docs.sh [--base REF]
#        sh scripts/check-design-docs.sh --print-allow
#        sh scripts/check-design-docs.sh --self-test
#   --base REF     the commit whose allow list (ii) compares with (default:
#                  the merge base of HEAD with main, or with origin/main when
#                  there is no main; (ii) is skipped when neither exists)
#   --print-allow  print the allow-list items of the tree as it is (every
#                  measure over its budget with its value), for the first list
#   --self-test    check the script on the fixtures under
#                  scripts/check-design-docs-fixtures/ in temporary git
#                  repositories, and remove them
#
# Exit 0 when every document is within its budget or its item, 1 when one is
# not or the allow list breaks (i) or (ii) (each document, its value and the
# limit go to stderr), 2 on a usage error, a malformed allow list, a base that
# is not a commit, or no docs/design.
set -eu

me=check-design-docs
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

# The budgets of docs/development/documents.md, section "design".
map_limit=30720
concept_limit=16384
line_limit=1024
task_limit=0
concept_docs="docs/design/overview.md docs/design/supervisor-lifecycle.md"
allow_path=.config/design-docs-allow.toml

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-design-docs.sh [--base REF] | --print-allow | --self-test" >&2
  exit 2
}

# measure <mode> <allow> <base-allow or ""> <sizes>, with the documents as
# the remaining arguments. mode is check or print.
measure() {
  mode=$1 allowf=$2 basef=$3 sizesf=$4
  shift 4
  LC_ALL=C awk -v me="$me" -v mode="$mode" -v allowf="$allowf" -v basef="$basef" \
    -v sizesf="$sizesf" -v allowname="$allow_path" -v concept="$concept_docs" \
    -v map_limit="$map_limit" -v concept_limit="$concept_limit" \
    -v line_limit="$line_limit" -v task_limit="$task_limit" '
function perr(msg) { printf "%s: %s:%d: %s\n", me, (FILENAME == basef ? "the base " allowname : allowname), FNR, msg > "/dev/stderr"; malformed = 1 }
function fail(msg) { print me ": " msg > "/dev/stderr"; bad = 1 }
function close_item(w) {
  if (!initem) return
  initem = 0
  if (ipath == "") { perr("an item has no path"); return }
  if (!ikeys) { perr("item " ipath " has no size, line or tasks"); return }
  if ((w, ipath) in listed) { perr("item " ipath " is listed twice"); return }
  listed[w, ipath] = 1
  if (w == "head") hpaths[++nh] = ipath
  for (k in ival) { val[w, ipath, k] = ival[k]; has[w, ipath, k] = 1 }
}
# One line of an allow list (w is head or base).
function allow_line(w,    line, key, v) {
  line = $0
  sub(/^[ \t]+/, "", line)
  if (line == "" || substr(line, 1, 1) == "#") return
  if (line ~ /^\[\[doc\]\][ \t]*(#.*)?$/) { close_item(w); initem = 1; ipath = ""; ikeys = 0; split("", ival); return }
  if (!initem) { perr("a key outside a [[doc]] item"); return }
  if (line !~ /=/) { perr("not a key = value line"); return }
  key = line; sub(/[ \t]*=.*/, "", key)
  v = line; sub(/^[^=]*=[ \t]*/, "", v); sub(/[ \t]+#[^"]*$/, "", v); sub(/[ \t]+$/, "", v)
  if (key == "path") {
    if (ipath != "") { perr("path is given twice in one item"); return }
    if (v !~ /^"[^"\\]+"$/) { perr("path is not a string"); return }
    ipath = substr(v, 2, length(v) - 2)
  } else if (key == "size" || key == "line" || key == "tasks") {
    if (key in ival) { perr("key " key " is given twice in one item"); return }
    if (v !~ /^[0-9]+$/) { perr(key " is not a number"); return }
    ival[key] = v + 0; ikeys++
  } else perr("unknown key " key)
}
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
# Remove link targets: from "](" to the next ")", and after a leading "[name]: ".
function strip_links(s,    p, q, rest) {
  if (match(s, /^\[[^]]*\]: /)) s = substr(s, 1, RLENGTH)
  rest = s; s = ""
  while ((p = index(rest, "](")) > 0) {
    q = index(substr(rest, p + 2), ")")
    if (q == 0) break
    s = s substr(rest, 1, p)
    rest = substr(rest, p + 2 + q)
  }
  return s rest
}
function is_count(after,    c) {
  c = substr(after, 1, 3)
  return c == "件" || c == "つ" || c == "個" || c == "本" || c == "回"
}
function count_tasks(s,    n, t) {
  s = strip_links(strip_code(s))
  n = 0; t = s
  while (match(t, /(^|[^A-Za-z0-9_])[Tt][Aa][Ss][Kk][Ss]? ?#?[0-9]+/)) {
    t = substr(t, RSTART + RLENGTH)
    if (!is_count(t)) n++
    # the match ends in a digit, so a word right after it is not at a start
    t = "0" t
  }
  t = s
  while (match(t, /タスク ?#?[0-9]+/)) {
    t = substr(t, RSTART + RLENGTH)
    if (!is_count(t)) n++
  }
  return n
}
function body(line, nr,    len, t, n) {
  len = length(line)
  if (len > maxline[cur]) maxline[cur] = len
  if (len > line_limit) { nlong[cur]++; longnr[cur, nlong[cur]] = nr; longlen[cur, nlong[cur]] = len }
  t = line; sub(/^[ \t]+/, "", t)
  if (substr(t, 1, 3) == "```") { fence = !fence; return }
  if (fence) return
  n = count_tasks(line)
  if (n > 0) { tasks[cur] += n; ntl[cur]++; tlnr[cur, ntl[cur]] = nr; tlcnt[cur, ntl[cur]] = n }
}
function finish(    i) {
  if (cur == "") return
  if (infm) {
    # no closing ---: there is no frontmatter, so measure the whole file
    fmb = 0; fence = 0
    for (i = 1; i <= nb; i++) body(buf[i], i)
  }
  size[cur] = total[cur] - fmb
  if (size[cur] < 0) size[cur] = 0
  cur = ""
}
function check_metric(d, m, v, budget, unit,    lim, i, allowed) {
  allowed = (("head", d, m) in has)
  if (mode == "print") {
    if (v > budget) printf "%s = %d\n", m, v
    return
  }
  if (v <= budget) {
    if (allowed) fail(d ": " m " is " v " " unit ", within the budget of " budget "; remove " m " from its item in " allowname)
    return
  }
  if (allowed && v <= val["head", d, m]) return
  lim = allowed ? val["head", d, m] : budget
  why = allowed ? "the limit of its item in " allowname : "the budget"
  if (m == "line") {
    for (i = 1; i <= nlong[d]; i++)
      if (longlen[d, i] > lim) fail(d ":" longnr[d, i] ": a line of " longlen[d, i] " bytes, over " why " of " lim " bytes")
  } else if (m == "tasks") {
    for (i = 1; i <= ntl[d]; i++) fail(d ":" tlnr[d, i] ": " tlcnt[d, i] " task number(s)")
    fail(d ": " v " task numbers in the body, over " why " of " lim)
  } else fail(d ": " v " bytes without the frontmatter, over " why " of " lim " bytes")
}
BEGIN {
  n = split(concept, c, " ")
  for (i = 1; i <= n; i++) isconcept[c[i]] = 1
}
FILENAME == allowf { aw = "head"; allow_line(aw); next }
FILENAME == basef { if (aw != "base") { close_item(aw); aw = "base" } allow_line(aw); next }
FILENAME == sizesf {
  if (aw != "") { close_item(aw); aw = "" }
  split($0, a, "\t"); docs[++nd] = a[1]; total[a[1]] = a[2] + 0; isdoc[a[1]] = 1
  next
}
{
  if (FILENAME != cur) {
    finish()
    cur = FILENAME; seen[cur] = 1; fence = 0; fmb = 0; nb = 0; infm = 0
    if ($0 == "---") { infm = 1; fmb = length($0) + 1; buf[++nb] = $0; next }
  }
  if (infm) {
    fmb += length($0) + 1; buf[++nb] = $0
    if ($0 == "---") { infm = 0; nb = 0 }
    next
  }
  body($0, FNR)
}
END {
  finish()
  # with no document under docs/design the sizes file is empty
  if (aw != "") close_item(aw)
  if (malformed) exit 2
  for (i = 1; i <= nd; i++) {
    d = docs[i]
    if (!(d in seen)) size[d] = total[d]
    if (mode == "print") {
      if (size[d] <= (d in isconcept ? concept_limit : map_limit) && maxline[d] + 0 <= line_limit && tasks[d] + 0 <= task_limit) continue
      printf "\n[[doc]]\npath = \"%s\"\n", d
    }
    check_metric(d, "size", size[d], (d in isconcept) ? concept_limit : map_limit, "bytes")
    check_metric(d, "line", maxline[d] + 0, line_limit, "bytes")
    check_metric(d, "tasks", tasks[d] + 0, task_limit, "task numbers")
  }
  if (mode == "print") exit 0
  for (i = 1; i <= nh; i++)
    if (!(hpaths[i] in isdoc)) fail(hpaths[i] ": in " allowname " but not a document under docs/design; remove its item")
  if (basef != "") {
    for (i = 1; i <= nh; i++) {
      p = hpaths[i]
      split("size line tasks", ms, " ")
      for (j = 1; j <= 3; j++) {
        m = ms[j]
        if (!(("head", p, m) in has)) continue
        if (!(("base", p, m) in has)) fail(p ": " m " = " val["head", p, m] " is in " allowname " but not in the base list; an item may not be added")
        else if (val["head", p, m] > val["base", p, m]) fail(p ": " m " = " val["head", p, m] " in " allowname " is larger than the base list'"'"'s " val["base", p, m] "; an item may not be raised")
      }
    }
  }
  if (bad) {
    print me ": the budgets and how to fix: docs/development/documents.md, section design (split the document or move the details to doc comments next to the definitions; an allow-list item may only go down or out)" > "/dev/stderr"
    exit 1
  }
  printf "%s: %d documents under docs/design, %d in %s: ok\n", me, nd, nh, allowname
}' "$allowf" ${basef:+"$basef"} "$sizesf" "$@"
}

run() {
  mode=$1
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
  cd "$root"
  if [ ! -d docs/design ]; then
    echo "$me: docs/design not found under $root" >&2
    exit 2
  fi
  work=$(mktemp -d "${TMPDIR:-/tmp}/$me.XXXXXX")
  trap 'rm -rf "$work"' EXIT
  if [ -f "$allow_path" ]; then cp "$allow_path" "$work/allow"; else : >"$work/allow"; fi

  basef=""
  if [ "$mode" = check ]; then
    if [ -n "$base" ]; then
      git rev-parse --verify --quiet "$base^{commit}" >/dev/null || {
        echo "$me: not a commit: $base" >&2
        exit 2
      }
    else
      for ref in main origin/main; do
        if git rev-parse --verify --quiet "$ref^{commit}" >/dev/null 2>&1; then
          base=$(git merge-base "$ref" HEAD 2>/dev/null) || base=""
          [ -n "$base" ] || echo "$me: no merge base of HEAD with $ref"
          break
        fi
      done
    fi
    if [ -z "$base" ]; then
      echo "$me: no base (no main or origin/main): the allow list is not compared with a base"
    elif git cat-file -e "$base:$allow_path" 2>/dev/null; then
      git show "$base:$allow_path" >"$work/base" || exit 2
      basef=$work/base
    else
      echo "$me: the base $base has no $allow_path: the allow list is not compared with a base"
    fi
  fi

  find docs/design -type f -name '*.md' | LC_ALL=C sort >"$work/docs"
  : >"$work/sizes"
  while IFS= read -r doc; do
    printf '%s\t%s\n' "$doc" "$(wc -c <"$doc" | tr -d '[:space:]')" >>"$work/sizes"
  done <"$work/docs"
  # One path per line, so a path with a blank stays one argument.
  oldifs=$IFS
  IFS='
'
  set -f
  # shellcheck disable=SC2046
  set -- $(cat "$work/docs")
  set +f
  IFS=$oldifs
  set +e
  measure "$mode" "$work/allow" "$basef" "$work/sizes" "$@"
  status=$?
  set -e
  exit "$status"
}

# self_test builds small git repositories from the fixtures and checks the
# script on them, case by case.
self_test() {
  fixtures=$here/check-design-docs-fixtures
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  # fresh: a repository on main with one small document committed.
  fresh() {
    rm -rf "$r"
    mkdir -p "$r/docs/design" "$r/.config"
    g init -q
    g symbolic-ref HEAD refs/heads/main
    printf '# ok\n' >"$r/docs/design/ok.md"
    g add -A && g commit -q -m base
  }
  # pad FILE BYTES appends lines of x until FILE has BYTES bytes.
  pad() {
    cur=$(wc -c <"$1" | tr -d '[:space:]')
    awk -v n=$(($2 - cur)) 'BEGIN { x = "x"; while (length(x) < 79) x = x "x"
      while (n >= 80) { print x; n -= 80 }
      if (n > 0) { for (i = 1; i < n; i++) printf "x"; printf "\n" } }' >>"$1"
  }
  # line N STR prints one line of STR repeated N times.
  line() { awk -v n="$1" -v s="$2" 'BEGIN { for (i = 0; i < n; i++) printf "%s", s; printf "\n" }'; }
  allow() { cat >"$r/.config/design-docs-allow.toml"; }
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
      sed 's/^/  /' "$st/err" >&2
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

  # The boundaries of the task numbers: what is left out, then what counts.
  fresh
  cp "$fixtures/tasks-left-out.md" "$r/$d/"
  check tasks_left_out 0
  fresh
  cp "$fixtures/tasks-counted.md" "$r/$d/"
  check tasks_counted 1
  has link_text_counted "$d/tasks-counted.md:7: 1 task number(s)"
  has unclosed_backtick_counted "$d/tasks-counted.md:8: 1 task number(s)"
  has count_word_only_its_match "$d/tasks-counted.md:9: 1 task number(s)"
  has japanese_counted "$d/tasks-counted.md:10: 1 task number(s)"
  has case_hash_plural_counted "$d/tasks-counted.md:11: 2 task number(s)"
  has after_the_fence_counted "$d/tasks-counted.md:16: 1 task number(s)"
  has total "$d/tasks-counted.md: 7 task numbers in the body, over the budget of 0"
  lacks fence_not_counted "$d/tasks-counted.md:13:"
  fresh
  cp "$fixtures/frontmatter-trailing-space.md" "$fixtures/frontmatter-unclosed.md" "$r/$d/"
  check frontmatter_not_recognized 1
  has trailing_space_is_body "$d/frontmatter-trailing-space.md:2: 1 task number(s)"
  has unclosed_is_body "$d/frontmatter-unclosed.md:2: 1 task number(s)"

  # The frontmatter: its size, long lines and task numbers are not measured.
  fresh
  f=$r/$d/frontmatter.md
  { echo ---; echo 'note: task 1942'; line 2000 y; } >"$f"
  pad "$f" 40000
  printf -- '---\n# body\n' >>"$f"
  fmb=$(($(wc -c <"$f") - 7))
  pad "$f" $((fmb + 30720))
  check frontmatter_left_out 0
  pad "$f" $((fmb + 30721))
  check frontmatter_body_one_byte_over 1
  has body_size_without_frontmatter "$d/frontmatter.md: 30721 bytes without the frontmatter, over the budget of 30720 bytes"

  # Sizes: concept and map documents, and the bytes of a fenced code block.
  fresh
  : >"$r/$d/map.md" && pad "$r/$d/map.md" 30720
  : >"$r/$d/supervisor-lifecycle.md" && pad "$r/$d/supervisor-lifecycle.md" 16384
  mkdir -p "$r/$d/sub" && : >"$r/$d/sub/overview.md" && pad "$r/$d/sub/overview.md" 20000
  check sizes_at_the_budgets 0
  pad "$r/$d/map.md" 30721
  : >"$r/$d/overview.md" && pad "$r/$d/overview.md" 16385
  check sizes_one_byte_over 1
  has map_over "$d/map.md: 30721 bytes without the frontmatter, over the budget of 30720 bytes"
  has concept_over "$d/overview.md: 16385 bytes without the frontmatter, over the budget of 16384 bytes"
  lacks nested_overview_is_a_map "$d/sub/overview.md"
  fresh
  { echo '```text'; } >"$r/$d/fenced.md"
  pad "$r/$d/fenced.md" 30721
  check fence_counts_toward_size 1
  has fence_size "$d/fenced.md: 30721 bytes"

  # Line lengths in bytes, without the newline, fences included.
  fresh
  line 1024 a >"$r/$d/lines.md"
  line 341 'あ' >>"$r/$d/lines.md"
  check lines_at_the_budget 0
  fresh
  { echo '# lines'; line 1025 a; line 342 'あ'; echo '```'; line 1025 b; echo '```'; } >"$r/$d/lines.md"
  check lines_over 1
  has ascii_line_over "$d/lines.md:2: a line of 1025 bytes, over the budget of 1024 bytes"
  has multibyte_line_over "$d/lines.md:3: a line of 1026 bytes, over the budget of 1024 bytes"
  has fence_line_over "$d/lines.md:5: a line of 1025 bytes, over the budget of 1024 bytes"

  # (i) The allow list against the tree.
  fresh
  : >"$r/$d/big.md" && pad "$r/$d/big.md" 40000
  { echo '# t'; echo 'task 1 and task 2'; line 1500 a; } >"$r/$d/old.md"
  allow <<'EOF'
# fixture
[[doc]]
path = "docs/design/big.md"
size = 50000

[[doc]]
path = "docs/design/old.md"
line = 1500   # the longest line
tasks = 2
EOF
  check allow_over_the_budget_and_shrunk 0
  allow <<'EOF'
[[doc]]
path = "docs/design/big.md"
size = 39999

[[doc]]
path = "docs/design/old.md"
line = 1499
tasks = 1
EOF
  check allow_values_exceeded 1
  has size_over_item "$d/big.md: 40000 bytes without the frontmatter, over the limit of its item in .config/design-docs-allow.toml of 39999 bytes"
  has line_over_item "$d/old.md:3: a line of 1500 bytes, over the limit of its item in .config/design-docs-allow.toml of 1499 bytes"
  has tasks_over_item "$d/old.md: 2 task numbers in the body, over the limit of its item in .config/design-docs-allow.toml of 1"
  allow <<'EOF'
[[doc]]
path = "docs/design/big.md"
size = 50000
line = 2000

[[doc]]
path = "docs/design/old.md"
line = 1500
tasks = 2

[[doc]]
path = "docs/design/gone.md"
size = 40000
EOF
  check allow_stale_items 1
  has within_budget_item "$d/big.md: line is 79 bytes, within the budget of 1024; remove line from its item"
  has missing_document "docs/design/gone.md: in .config/design-docs-allow.toml but not a document under docs/design"
  fresh
  : >"$r/$d/big.md" && pad "$r/$d/big.md" 40000
  check not_in_the_allow_list 1
  has not_listed "$d/big.md: 40000 bytes without the frontmatter, over the budget of 30720 bytes"
  printf '[[doc]]\npath = "docs/design/big.md"\nsize = 50000\nwidth = 3\n' | allow
  check malformed_allow_list 2
  printf '[[doc]]\npath = "docs/design/big.md"\n' | allow
  check item_without_a_measure 2

  # (ii) The allow list against the base's.
  fresh
  : >"$r/$d/big.md" && pad "$r/$d/big.md" 40000
  : >"$r/$d/other.md" && pad "$r/$d/other.md" 35000
  printf '[[doc]]\npath = "docs/design/big.md"\nsize = 50000\n\n[[doc]]\npath = "docs/design/other.md"\nsize = 35000\n' | allow
  check no_base_list_skips_the_comparison 0
  has skipped "has no .config/design-docs-allow.toml"
  g add -A && g commit -q -m 'allow list'
  base=$(g rev-parse HEAD)
  check same_as_the_base 0 --base "$base"
  printf '[[doc]]\npath = "docs/design/big.md"\nsize = 45000\n' | allow
  : >"$r/$d/other.md" && pad "$r/$d/other.md" 1000
  check lowered_and_removed 0 --base "$base"
  printf '[[doc]]\npath = "docs/design/big.md"\nsize = 60000\n\n[[doc]]\npath = "docs/design/other.md"\nsize = 35000\n' | allow
  check raised 1 --base "$base"
  has raised_item "docs/design/big.md: size = 60000 in .config/design-docs-allow.toml is larger than the base list's 50000"
  { echo '# n'; echo 'task 9'; } >"$r/$d/new.md"
  { echo '# o'; line 1100 a; } >>"$r/$d/other.md"
  printf '[[doc]]\npath = "docs/design/big.md"\nsize = 50000\n\n[[doc]]\npath = "docs/design/other.md"\nsize = 35000\nline = 1100\n\n[[doc]]\npath = "docs/design/new.md"\ntasks = 1\n' | allow
  check added 1 --base "$base"
  has added_item "docs/design/new.md: tasks = 1 is in .config/design-docs-allow.toml but not in the base list"
  has added_key "docs/design/other.md: line = 1100 is in .config/design-docs-allow.toml but not in the base list"
  lacks base_values_kept "docs/design/big.md: size"
  # Without --base: the merge base with main.
  g checkout -q -b work
  g add -A && g commit -q -m 'raise on a branch'
  check default_base_is_the_merge_base_with_main 1
  has default_base_added "docs/design/new.md: tasks = 1 is in .config/design-docs-allow.toml but not in the base list"
  check base_not_a_commit 2 --base no-such-ref

  # An item of a gone document with no document left, and a path with a blank.
  fresh
  rm "$r/$d/ok.md"
  printf '[[doc]]\npath = "docs/design/gone.md"\nsize = 40000\n' | allow
  check gone_item_with_no_documents 1
  has gone_item "docs/design/gone.md: in .config/design-docs-allow.toml but not a document"
  rm "$r/.config/design-docs-allow.toml"
  printf '# a b\ntask 5\n' >"$r/$d/a b.md"
  check path_with_a_blank 1
  has blank_path "$d/a b.md:2: 1 task number(s)"

  # No docs/design: nothing to check.
  rm -rf "$r/docs"
  check no_docs_design 2

  if [ "$fail" -eq 0 ]; then echo "$me --self-test: ok"; else exit 1; fi
}

base=""
case "${1:-}" in
  --self-test)
    [ $# -eq 1 ] || usage "--self-test takes no other argument"
    self_test
    exit 0
    ;;
  --print-allow)
    [ $# -eq 1 ] || usage "--print-allow takes no other argument"
    run print
    ;;
esac

while [ $# -gt 0 ]; do
  case "$1" in
    --base) [ $# -ge 2 ] || usage "--base needs a revision"; base=$2; shift 2 ;;
    -h|--help) sed -n '2,44p' "$0"; exit 0 ;;
    *) usage "unknown argument: $1" ;;
  esac
done
run check
