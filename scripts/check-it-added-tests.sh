#!/bin/sh
# The boundary gate of the tests in tests/it (ADR-t3762-1): a #[test] fn that
# the diff from a base to the work tree adds to tests/it fails the check
# unless a declaration names the boundary it keeps. The rule on what may be
# declared is in docs/development/testing.md, section "判断と境界のtest": a
# test that only lays out the cases of a decision has no boundary and belongs
# in a unit test of src/. The script checks that each new test has an item
# and that the item's boundary is one of the set below; whether the reason
# writes a boundary is left to the test-rules agent
# (.dagq/agents/test-rules/AGENT.md).
#
# Usage: sh scripts/check-it-added-tests.sh [--base REF]
#        sh scripts/check-it-added-tests.sh --self-test
#   --base REF   the commit to compare the work tree with (default: the merge
#                base of HEAD with main, or with origin/main when there is no
#                main; HEAD when neither exists). A file git does not track
#                yet counts as added.
#   --self-test  check the script on the fixtures under
#                scripts/check-it-added-tests-fixtures/ in temporary git
#                repositories, and remove them
#
# A test is a fn with #[test] in a .rs file under tests/it. Its name is the
# full nextest name in dagq::it: the file's path under tests/it without .rs
# and /mod (a #[path] on a mod of tests/it/main.rs wins), the mods around the
# fn and the fn's name, joined by ::, e.g. runtime_resume::inner::a_case.
# Braces are counted with comments, strings, raw strings and char literals
# left out; a test a macro expands to is not seen.
#
# A test is new when its full name is in the work tree and nowhere in the
# base's tests/it. A new test is taken as moved, and passes without an item,
# when the base has a test of the same fn name whose full name is gone from
# the work tree (each such base test accounts for one move): moving a file or
# a test to another module changes no boundary. A fn renamed in place, or a
# test copied while the original stays, is new. A change to the body of a
# test already in the base, to a helper or to a fixture makes nothing new.
# Only the files the diff changes (and every file when tests/it/main.rs
# changes) are read: an unchanged file has the same tests on both sides.
#
# The declarations are the items of every .config/it-boundary.d/*.toml of
# the work tree together; a missing directory reads as empty. A new item goes
# in the file of the task that adds the test,
# .config/it-boundary.d/t<task ID>.toml, so that two tasks adding tests change
# no file in common. Each file is TOML items of the form
#   [[test]]
#   name = "<the full name>"
#   boundary = "<one of the set below>"
#   reason = "<why only an IT can check it: the boundary it keeps>"
#   task = <the ID of the task that added the item>
# with all four keys, each on one line ("..." or '...' strings, task a
# number; # comments). The boundary is one of the boundaries of
# testing.md's "判断と境界のtest":
#   SQLite, Git, プロセス, supervisorの配線, 復旧とadopt
# An item missing a key or with an empty one, a key it does not know or one
# key twice, a boundary outside the set, or two items of one name (in one
# file or in two) fail the check. An item whose name has no #[test] in the
# work tree's tests/it is warned about, without changing the exit status. A
# line that is not one of these forms makes the list unreadable.
#
# The tree checked is the git work tree of the cwd (`git rev-parse
# --show-toplevel`), so a copy of the script run elsewhere with the cwd in a
# worktree checks that worktree (docs/development/task-registration.md,
# section "推奨の組み合わせ"). Outside a git work tree it is the repository
# found from the script's own location. Only sh, git and python3 (3.9 or
# later, the standard library) are needed. python3 runs isolated (-I): the
# checked tree is the cwd, and without it a module file there (json.py,
# glob.py, ...) would be imported in place of the standard library's.
#
# Exit 0 when every new test has a valid item, 1 when one has none or an item
# is invalid (each test's name and file:line, or the item's file:line, go to
# stderr with how to fix it), 2 on a usage error, a base that is not a
# commit, a failing git, an unreadable list, or no tests/it.
set -eu

me=check-it-added-tests
here=$(cd "$(dirname "$0")" && pwd)
script=$here/$(basename "$0")

usage() {
  echo "$me: $1" >&2
  echo "usage: sh scripts/check-it-added-tests.sh [--base REF] | --self-test" >&2
  exit 2
}

# check <base> runs the gate in the cwd (the root) against the base commit.
check() {
  python3 -I - "$me" "$1" <<'PY'
import bisect, glob, json, os, re, subprocess, sys

me, base = sys.argv[1], sys.argv[2]
BOUNDARIES = ["SQLite", "Git", "プロセス", "supervisorの配線", "復旧とadopt"]


def die(msg):
    print("%s: %s" % (me, msg), file=sys.stderr)
    sys.exit(2)


def git(*args, data=None):
    p = subprocess.run(["git", "-c", "core.quotePath=false"] + list(args),
                       input=data, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode != 0:
        die("git %s failed: %s" % (" ".join(args), p.stderr.decode("utf-8", "replace").strip()))
    return p.stdout


def z(out):
    return [p.decode("utf-8", "surrogateescape") for p in out.split(b"\0") if p]


TOKEN = re.compile(r'''
  (?P<lc>//[^\n]*)
| (?P<bc>/\*)
| (?P<raw>(?<![A-Za-z0-9_])(?:br|cr|r)(?P<h>\#*)")
| (?P<str>(?<![A-Za-z0-9_])[bc]?"(?:[^"\\]|\\.)*")
| (?P<chr>'(?:[^'\\\n]|\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]{1,6}\}|.))')
| (?P<p>[{};()\[\]])
''', re.X | re.S)
BLOCK = re.compile(r"/\*|\*/")
MOD = re.compile(r"(?<![A-Za-z0-9_])mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*$")
TEST_ATTR = re.compile(r"#\s*\[\s*test\s*\]")
FN = re.compile(r"(?<![A-Za-z0-9_])fn\s+([A-Za-z_][A-Za-z0-9_]*)")
PATH_MOD = re.compile(r'#\[path\s*=\s*"([^"]+)"\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;')


def tests_of(text, prefix):
    """[(full name, line)] of the #[test] fns of a file's text."""
    nl = [m.start() for m in re.finditer("\n", text)]
    out, stack, buf, nest, pos = [], [], [], 0, 0
    seg = 0  # text position buf starts at
    n = len(text)
    while pos < n:
        m = TOKEN.search(text, pos)
        if not m:
            buf.append(text[pos:])
            break
        buf.append(text[pos:m.start()])
        kind = m.lastgroup
        end = m.end()
        if kind == "bc":
            depth, k = 1, end
            while depth:
                b = BLOCK.search(text, k)
                if not b:
                    k = n
                    break
                depth += 1 if b.group() == "/*" else -1
                k = b.end()
            end = k
        elif kind == "raw":
            close = '"' + m.group("h")
            k = text.find(close, end)
            end = n if k < 0 else k + len(close)
        if kind != "p":
            buf.append(" " * (end - m.start()))
            pos = end
            continue
        c = m.group()
        pos = end
        if c in "([":
            nest += 1
            buf.append(c)
            continue
        if c in ")]":
            nest = max(0, nest - 1)
            buf.append(c)
            continue
        if c == ";" and nest:
            buf.append(c)
            continue
        s = "".join(buf)
        if c == "{":
            mm = MOD.search(s)
            fm = FN.search(s) if TEST_ATTR.search(s) else None
            if mm:
                stack.append(("m", mm.group(1)))
            elif fm:
                line = bisect.bisect_left(nl, seg + fm.start()) + 1
                stack.append(("t", fm.group(1), line))
            else:
                stack.append(("o",))
        elif c == "}" and stack:
            top = stack.pop()
            if top[0] == "t":
                parts = ([prefix] if prefix else []) + [f[1] for f in stack if f[0] == "m"] + [top[1]]
                out.append(("::".join(parts), top[2]))
        buf, nest, seg = [], 0, end
    return out


def path_mods(main_text):
    """{file under tests/it: mod name} from the #[path] mods of main.rs."""
    res = {}
    for p, name in PATH_MOD.findall(main_text or ""):
        parts = []
        for x in p.split("/"):
            if x in ("", "."):
                continue
            if x == "..":
                if parts:
                    parts.pop()
                else:
                    parts = None
                    break
                continue
            parts.append(x)
        if parts:
            res["tests/it/" + "/".join(parts)] = name
    return res


def prefix_of(path, mods):
    if path in mods:
        return mods[path]
    p = path[len("tests/it/"):][:-len(".rs")]
    if p.endswith("/mod"):
        p = p[:-len("/mod")]
    if p == "main" or p == "mod":
        return ""
    return p.replace("/", "::")


def is_rs(p):
    return p.startswith("tests/it/") and p.endswith(".rs")


# The files that differ between the base and the work tree.
changed = set(p for p in z(git("diff", "-z", "--name-only", "--no-renames", "--no-ext-diff", "--no-textconv", base, "--", "tests/it")) if is_rs(p))
untracked = set(p for p in z(git("ls-files", "-z", "--others", "--exclude-standard", "--", "tests/it")) if is_rs(p))
changed |= untracked

base_blobs = {}
for entry in git("ls-tree", "-r", "-z", base, "--", "tests/it").split(b"\0"):
    if not entry:
        continue
    meta, path = entry.split(b"\t", 1)
    path = path.decode("utf-8", "surrogateescape")
    fields = meta.split()
    if fields[1] == b"blob" and is_rs(path):
        base_blobs[path] = fields[2].decode()
work_files = set(p for p in z(git("ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "tests/it"))
                 if is_rs(p) and os.path.isfile(p))


def read_blobs(paths):
    """{path: text} of the base's blobs, in one git cat-file."""
    paths = sorted(paths)
    if not paths:
        return {}
    data = git("cat-file", "--batch", data="".join(base_blobs[p] + "\n" for p in paths).encode())
    res, k = {}, 0
    for p in paths:
        nl = data.index(b"\n", k)
        size = int(data[k:nl].split()[2])
        res[p] = data[nl + 1:nl + 1 + size].decode("utf-8", "replace")
        k = nl + 1 + size + 1
    return res


def read_work(p):
    with open(p, "rb") as f:
        return f.read().decode("utf-8", "replace")


main = "tests/it/main.rs"
if main in changed:
    base_read, work_read = set(base_blobs), set(work_files)
else:
    base_read = set(p for p in changed if p in base_blobs)
    work_read = set(p for p in changed if p in work_files)
base_text = read_blobs(base_read | ({main} if main in base_blobs else set()))
base_mods = path_mods(base_text.get(main))
work_mods = path_mods(read_work(main) if main in work_files else "")

base_tests = {}
for p in sorted(base_read):
    for name, line in tests_of(base_text[p], prefix_of(p, base_mods)):
        base_tests[name] = p
work_tests = {}
for p in sorted(work_read):
    for name, line in tests_of(read_work(p), prefix_of(p, work_mods)):
        work_tests.setdefault(name, "%s:%d" % (p, line))

new = [n for n in work_tests if n not in base_tests]
gone = {}
for n in base_tests:
    if n not in work_tests:
        gone.setdefault(n.rsplit("::", 1)[-1], []).append(n)


# The declarations.
ITEM = re.compile(r"^\[\[test\]\]\s*(#.*)?$")
KEY = re.compile(r'''^([A-Za-z0-9_-]+)\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*'|[0-9]+)\s*(#.*)?$''')
bad = 0


def fail(msg):
    global bad
    print("%s: %s" % (me, msg), file=sys.stderr)
    bad += 1


items = []
for f in sorted(glob.glob(".config/it-boundary.d/*.toml")):
    try:
        lines = read_work(f).split("\n")
    except OSError as e:
        die("cannot read %s: %s" % (f, e))
    item = None
    for i, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        where = "%s:%d" % (f, i)
        if ITEM.match(line):
            item = {"_where": where}
            items.append(item)
            continue
        km = KEY.match(line)
        if not km:
            die("%s: not an item line ([[test]] or key = value on one line): %s" % (where, line))
        if item is None:
            die("%s: a key outside a [[test]] item" % where)
        key, val = km.group(1), km.group(2)
        if key in item:
            fail("%s: key %s is given twice in one item" % (where, key))
            continue
        if key not in ("name", "boundary", "reason", "task"):
            fail("%s: unknown key %s" % (where, key))
            continue
        if key == "task":
            if not val.isdigit():
                fail("%s: task is not a number" % where)
                continue
        elif val[0] == '"':
            try:
                val = json.loads(val)
            except ValueError:
                die("%s: %s is not a string" % (where, key))
        elif val[0] == "'":
            val = val[1:-1]
        else:
            fail("%s: %s is not a string" % (where, key))
            continue
        item[key] = val

declared = {}
for item in items:
    where = item["_where"]
    missing = [k for k in ("name", "boundary", "reason", "task") if not str(item.get(k, "")).strip()]
    if missing:
        fail("%s: the item %s has no %s" % (where, item.get("name", ""), ", ".join(missing)))
    if "boundary" in item and item["boundary"] not in BOUNDARIES:
        fail("%s: boundary %r of %s is not one of %s (docs/development/testing.md, 判断と境界のtest);"
             " a test with no boundary goes to a unit test of src/" % (where, item["boundary"], item.get("name", ""), ", ".join(BOUNDARIES)))
    name = item.get("name")
    if name:
        if name in declared:
            fail("%s: %s is declared twice (first at %s)" % (where, name, declared[name]))
        else:
            declared[name] = where

missing = 0
for n in new:
    leaf = n.rsplit("::", 1)[-1]
    if n not in declared and gone.get(leaf):
        print("%s: %s at %s: moved from %s" % (me, n, work_tests[n], gone[leaf].pop(0)))
        continue
    if n in declared:
        continue
    print("%s: %s at %s: a new test in tests/it with no boundary declared" % (me, n, work_tests[n]), file=sys.stderr)
    missing += 1

if declared and (work_read != work_files):
    # Declarations of tests in unchanged files: read the rest of the tree once.
    for p in sorted(work_files - work_read):
        for name, line in tests_of(read_work(p), prefix_of(p, work_mods)):
            work_tests.setdefault(name, "%s:%d" % (p, line))
for name, where in declared.items():
    if name not in work_tests:
        print("%s: warning: %s: %s has no #[test] in tests/it" % (me, where, name), file=sys.stderr)

if missing:
    print("%s: %d new test(s) in tests/it with no boundary: move the cases of a decision to a unit test of a"
          " side-effect-free fn in src/, or, when the test keeps a boundary (%s), declare it in"
          " .config/it-boundary.d/t<task ID>.toml (docs/development/testing.md, 判断と境界のtest; the format is in"
          " the header of scripts/check-it-added-tests.sh)" % (me, missing, ", ".join(BOUNDARIES)), file=sys.stderr)
if missing or bad:
    sys.exit(1)
print("%s: %d new test(s) in tests/it since %s, each declared or moved: ok" % (me, len(new), base[:12]))
PY
}

run() {
  root=$(git rev-parse --show-toplevel 2>/dev/null) || root=$(cd "$here/.." && pwd)
  cd "$root"
  if [ ! -d tests/it ]; then
    echo "$me: no tests/it under $root" >&2
    exit 2
  fi
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
    echo "$me: $root is not a git work tree" >&2
    exit 2
  }
  if [ -n "$base" ]; then
    base=$(git rev-parse --verify --quiet "$base^{commit}") || {
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
      echo "$me: no merge base of HEAD with main or origin/main: the tests added since HEAD are checked"
      base=$(git rev-parse HEAD) || exit 2
    fi
  fi
  check "$base"
}

# self_test builds a git repository per case from the fixtures and checks
# the script on it. A case is a directory with an `after` tree laid over the
# base (its own `base`, or the shared one), an optional `remove` list of
# paths to delete first, an optional `uses` naming another case whose
# `after` is laid first, and `expect`: the exit status on the first line,
# then "+TEXT" for a text the output must hold and "-TEXT" for one it must
# not.
self_test() {
  fixtures=$here/check-it-added-tests-fixtures
  st=$(mktemp -d "${TMPDIR:-/tmp}/$me-self-test.XXXXXX")
  trap 'rm -rf "$st"' EXIT
  r=$st/repo
  fail=0
  g() { git -C "$r" -c user.name=self-test -c user.email=self-test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"; }
  fresh() {
    rm -rf "$r"
    mkdir -p "$r"
    cp -R "$1/." "$r/"
    g init -q
    g symbolic-ref HEAD refs/heads/main
    g add -A && g commit -q -m base
  }
  # expect LABEL WANT [ARGS...] runs the script in the repository.
  expect() {
    label=$1 want=$2
    shift 2
    set +e
    (cd "$r" && sh "$script" "$@") </dev/null >"$st/out" 2>&1
    got=$?
    set -e
    if [ "$got" -eq "$want" ]; then
      echo "$me --self-test: $label: exit $got: ok"
    else
      echo "$me --self-test: $label: exit $got, want $want" >&2
      sed 's/^/  /' "$st/out" >&2
      fail=1
    fi
  }
  has() {
    if ! grep -qF -- "$2" "$st/out"; then
      echo "$me --self-test: $1: FAILED (want \"$2\" in the output)" >&2
      sed 's/^/  /' "$st/out" >&2
      fail=1
    fi
  }
  lacks() {
    if grep -qF -- "$2" "$st/out"; then
      echo "$me --self-test: $1: FAILED (\"$2\" not wanted in the output)" >&2
      fail=1
    fi
  }

  for c in "$fixtures"/cases/*/; do
    c=${c%/}
    name=$(basename "$c")
    if [ -d "$c/base" ]; then fresh "$c/base"; else fresh "$fixtures/base"; fi
    if [ -f "$c/remove" ]; then
      while IFS= read -r p; do [ -n "$p" ] && rm -rf "${r:?}/$p"; done <"$c/remove"
    fi
    if [ -f "$c/uses" ]; then cp -R "$fixtures/cases/$(cat "$c/uses")/after/." "$r/"; fi
    if [ -d "$c/after" ]; then cp -R "$c/after/." "$r/"; fi
    want=$(sed -n 1p "$c/expect")
    expect "$name" "$want"
    sed -n '2,$p' "$c/expect" | while IFS= read -r line; do
      case $line in
        +*) has "$name" "${line#+}" ;;
        -*) lacks "$name" "${line#-}" ;;
      esac
    done >>"$st/checks" 2>&1
    if [ -s "$st/checks" ]; then
      cat "$st/checks" >&2
      : >"$st/checks"
      fail=1
    fi
    # The same work staged reads the same.
    g add -A
    expect "$name staged" "$want"
  done

  # The base: the merge base with main, --base, HEAD without main.
  fresh "$fixtures/base"
  base_commit=$(g rev-parse HEAD)
  g checkout -q -b work
  cp -R "$fixtures/cases/new_without_declaration/after/." "$r/"
  g add -A && g commit -q -m 'a test on a branch'
  expect default_base_is_the_merge_base_with_main 1
  has committed_test "alpha::a_decision_case"
  expect explicit_base 1 --base "$base_commit"
  expect base_at_branch_head 0 --base HEAD
  g branch -q -D main
  expect no_main_checks_since_head 0
  has no_main_message "the tests added since HEAD are checked"
  expect base_not_a_commit 2 --base no-such-ref
  expect unknown_argument 2 --no-such-flag
  expect base_without_a_revision 2 --base
  # A module file in the checked tree is not imported in place of the
  # standard library's.
  for m in json glob bisect subprocess re os; do
    printf 'raise SystemExit("the tree'"'"'s %s.py was imported")\n' "$m" >"$r/$m.py"
  done
  expect modules_of_the_tree_not_imported 0
  lacks modules_of_the_tree_not_imported "was imported"
  rm -f "$r"/*.py
  rm -rf "$r/tests/it"
  expect no_tests_it 2

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
