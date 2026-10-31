#!/bin/sh
# The boundary gate of the tests in tests/it (ADR-t3762-1, amended by
# ADR-t3802-1): a tests/it keeps a boundary in one representative test, and
# the boundaries are a named registry. Against the diff from a base to the
# work tree the check fails
#   (a) a #[test] fn the diff adds to tests/it that is not moved and is not
#       the representative of an id of the registry: replace the
#       representative of an existing id with it (and remove the one it
#       replaces, give that one another id or move it to a unit test), or add
#       a new id;
#   (b) an id two items give (a second test of one boundary, R), and a test
#       that two ids have as their representative;
#   (c) a module (a file of tests/it) whose #[test]s grow over the base's by
#       more than it is allowed: the new ids whose representative is in it,
#       plus the ids whose representative the diff moved into it, plus the
#       tests moved into it, less the ids whose representative the diff moved
#       out of it. Both sides are counted from the trees on each run, so no
#       number is kept in a file two tasks would change. A replaced
#       representative left in place counts one over.
# The rule on what may have an id is in docs/development/testing.md, section
# "判断と境界のtest": a test that only lays out the cases of a decision has no
# boundary and belongs in a unit test of src/. The script checks the form of
# the items and the counts. That a new id is approved is not the script's: the
# item names the task that adds it, whose acceptance or description names the
# id (plan review looks), and whether the reason writes a boundary is left to
# the test-rules agent (.dagq/agents/test-rules/AGENT.md).
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
# base's tests/it. A new test is taken as moved, and passes without an id,
# when it represents no id and the base has a test of the same fn name whose
# full name is gone from the work tree (each such base test accounts for one
# move; a replaced representative accounts for none): moving a file or a
# test to another module changes no boundary. A fn
# renamed in place, or a test copied while the original stays, is new. A
# change to the body of a test already in the base, to a helper or to a
# fixture makes nothing new. Only the files the diff changes (and every file
# when tests/it/main.rs changes) are counted: an unchanged file has the same
# tests on both sides. A full name twice in one file (a #[cfg] pair) counts
# once.
#
# The registry is the items of every .config/it-boundaries.d/*.toml of the
# work tree together; a missing directory reads as empty. A new item goes in
# the file of the task that adds it, .config/it-boundaries.d/t<task ID>.toml,
# so that two tasks change no file in common. Each file is TOML items of the
# form
#   [[boundary]]
#   id = "<lowercase words joined by -, e.g. queue-file-across-connections>"
#   kind = "<one of the set below>"
#   test = "<the full name of the representative test>"
#   reason = "<why only an IT can check it: the boundary it keeps>"
#   task = <the ID of the task that added the item>
# with all five keys, each on one line ("..." or '...' strings, task a
# number; # comments). The kind is one of the boundaries of testing.md's
# "判断と境界のtest":
#   SQLite, Git, プロセス, supervisorの配線, 復旧とadopt
# An id is new when the base's registry has no item of it, and its
# representative is replaced when the base's item of it names another test.
# To replace a representative, change the test of the id's item where it is.
#
# The old place, for a while: .config/it-boundary.d/*.toml is read too, with
# items of the old form
#   [[test]]
#   name = "<the full name>"
#   boundary = "<one of the set above>"
#   reason = "<the boundary it keeps>"
#   task = <the ID of the task that added the item>
# An old item reads as one id, t<task>:<name> (e.g.
# t3760:runtime_verify_retry::a_case), whose representative is the named
# test, so a run of a task registered under the old rule passes (a) and
# counts one new id in (c). The derived ids have a : and so never meet an id
# of the registry. New items go in the registry, not in the old place.
# Reading the old place stops with a later task that retires it, not here.
#
# An item missing a key or with an empty one, a key it does not know or one
# key twice, a kind (boundary) outside the set, an id not of the form, an id
# given twice or a test named by two items (in one file or in two) fail the
# check. An item whose test has no #[test] in the work tree's tests/it is
# warned about, without changing the exit status. A line that is not one of
# these forms, or the other place's item table, makes the list unreadable.
# The base's registry is read only for its ids and their representatives.
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
# Exit 0 when every new test is kept and the items and counts hold, 1 when
# one does not (each test's name and file:line, the module, or the item's
# file:line, go to stderr with how to fix it), 2 on a usage error, a base
# that is not a commit, a failing git, an unreadable list, or no tests/it.
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


def read_blobs(blobs):
    """{path: text} of {path: blob}, in one git cat-file."""
    paths = sorted(blobs)
    if not paths:
        return {}
    data = git("cat-file", "--batch", data="".join(blobs[p] + "\n" for p in paths).encode())
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
base_text = read_blobs({p: base_blobs[p] for p in base_read | ({main} if main in base_blobs else set())})
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


# The registry.
TABLE = re.compile(r"^\[\[([A-Za-z0-9_-]+)\]\]\s*(#.*)?$")
KEY = re.compile(r'''^([A-Za-z0-9_-]+)\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*'|[0-9]+)\s*(#.*)?$''')
ID = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")
NEW_DIR, OLD_DIR = ".config/it-boundaries.d", ".config/it-boundary.d"
# (directory, table, keys, whether its items are of the old format)
DIRS = [(NEW_DIR, "boundary", ("id", "kind", "test", "reason", "task"), False),
        (OLD_DIR, "test", ("name", "boundary", "reason", "task"), True)]
bad = 0


def fail(msg):
    global bad
    print("%s: %s" % (me, msg), file=sys.stderr)
    bad += 1


def quiet(msg):
    pass


def parse(f, text, table, keys, report, strict):
    """The items of a registry file, each a dict with _where; a key that is
    given twice, unknown or of the wrong type goes to report. A line that is
    not an item line ends in die when strict, and is skipped when not."""
    items, item = [], None
    for i, raw in enumerate(text.split("\n"), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        where = "%s:%d" % (f, i)
        tm = TABLE.match(line)
        if tm and tm.group(1) == table:
            item = {"_where": where}
            items.append(item)
            continue
        km = KEY.match(line)
        if tm or not km or item is None:
            if not strict:
                item = None
                continue
            if tm:
                die("%s: [[%s]] is not an item of %s, whose items are [[%s]]" % (where, tm.group(1), os.path.dirname(f), table))
            if not km:
                die("%s: not an item line ([[%s]] or key = value on one line): %s" % (where, table, line))
            die("%s: a key outside a [[%s]] item" % (where, table))
        key, val = km.group(1), km.group(2)
        if key in item:
            report("%s: key %s is given twice in one item" % (where, key))
            continue
        if key not in keys:
            report("%s: unknown key %s" % (where, key))
            continue
        if key == "task":
            if not val.isdigit():
                report("%s: task is not a number" % where)
                continue
        elif val[0] == '"':
            try:
                val = json.loads(val)
            except ValueError:
                if strict:
                    die("%s: %s is not a string" % (where, key))
                continue
        elif val[0] == "'":
            val = val[1:-1]
        else:
            report("%s: %s is not a string" % (where, key))
            continue
        item[key] = val
    return items


def entries(f, text, table, keys, old, report, strict):
    """The registry's entries of a file: {id, kind, test, where}. An old item
    reads as the id t<task>:<name> with the name as its representative."""
    out = []
    for item in parse(f, text, table, keys, report, strict):
        where = item["_where"]
        missing = [k for k in keys if not str(item.get(k, "")).strip()]
        if missing:
            report("%s: the item %s has no %s" % (where, item.get("id", item.get("name", "")), ", ".join(missing)))
        kind = item.get("boundary" if old else "kind")
        test = item.get("name" if old else "test")
        if old:
            ident = "t%s:%s" % (item["task"], test) if item.get("task") and test else None
        else:
            ident = item.get("id")
            if ident and not ID.match(ident):
                report("%s: id %r is not lowercase words joined by - (e.g. queue-file-across-connections)" % (where, ident))
                ident = None
        if kind is not None and kind not in BOUNDARIES:
            report("%s: %s %r of %s is not one of %s (docs/development/testing.md, 判断と境界のtest);"
                   " a test with no boundary goes to a unit test of src/"
                   % (where, "boundary" if old else "kind", kind, test or ident or "", ", ".join(BOUNDARIES)))
        if ident and test:
            out.append({"id": ident, "kind": kind, "test": test, "where": where})
    return out


# The work tree's registry, checked.
registry = []
for d, table, keys, old in DIRS:
    for f in sorted(glob.glob(d + "/*.toml")):
        try:
            text = read_work(f)
        except OSError as e:
            die("cannot read %s: %s" % (f, e))
        registry += entries(f, text, table, keys, old, fail, True)

# The base's registry, read leniently: only its ids and representatives.
base_reg = {}
listing = {}
for entry in git("ls-tree", "-r", "-z", base, "--", NEW_DIR, OLD_DIR).split(b"\0"):
    if not entry:
        continue
    meta, path = entry.split(b"\t", 1)
    path = path.decode("utf-8", "surrogateescape")
    fields = meta.split()
    if fields[1] == b"blob" and path.endswith(".toml") and os.path.dirname(path) in (NEW_DIR, OLD_DIR):
        listing[path] = fields[2].decode()
for path, text in sorted(read_blobs(listing).items()):
    d, table, keys, old = [x for x in DIRS if x[0] == os.path.dirname(path)][0]
    for e in entries(path, text, table, keys, old, quiet, False):
        base_reg.setdefault(e["id"], e["test"])

# One id has one representative (a second is R), and one test represents one
# id.
by_id, by_test = {}, {}
for e in registry:
    first = by_id.get(e["id"])
    if first:
        fail("%s: the id %s has a second test %s (first %s at %s): one boundary is kept by one test (R);"
             " move the cases of the second to a unit test or give it its own id"
             % (e["where"], e["id"], e["test"], first["test"], first["where"]))
        continue
    by_id[e["id"]] = e
    if e["test"] in by_test:
        fail("%s: %s is declared twice (first at %s)" % (e["where"], e["test"], by_test[e["test"]]["where"]))
        continue
    by_test[e["test"]] = e
new_ids = [i for i in by_id if i not in base_reg]
replaced = [i for i in by_id if i in base_reg and base_reg[i] != by_id[i]["test"]]

# (a) A new test is the representative of an id, or moved. A replaced
# representative gone from the work tree went to its replacement, and so
# accounts for no move.
for i in replaced:
    lst = gone.get(base_reg[i].rsplit("::", 1)[-1], [])
    if base_reg[i] in lst:
        lst.remove(base_reg[i])
missing = 0
moved_in = {}
for n in new:
    leaf = n.rsplit("::", 1)[-1]
    if n not in by_test and gone.get(leaf):
        print("%s: %s at %s: moved from %s" % (me, n, work_tests[n], gone[leaf].pop(0)))
        f = work_tests[n].rsplit(":", 1)[0]
        moved_in[f] = moved_in.get(f, 0) + 1
        continue
    if n in by_test:
        continue
    print("%s: %s at %s: a new test in tests/it that no boundary id has as its representative" % (me, n, work_tests[n]), file=sys.stderr)
    missing += 1

if registry and (work_read != work_files):
    # Representatives in unchanged files: read the rest of the tree once.
    for p in sorted(work_files - work_read):
        for name, line in tests_of(read_work(p), prefix_of(p, work_mods)):
            work_tests.setdefault(name, "%s:%d" % (p, line))
for e in by_id.values():
    if e["test"] not in work_tests:
        print("%s: warning: %s: %s has no #[test] in tests/it" % (me, e["where"], e["test"]), file=sys.stderr)


def file_of(name, base_side):
    if base_side and name in base_tests:
        return base_tests[name]
    w = work_tests.get(name)
    return w.rsplit(":", 1)[0] if w else None


# (c) The ratchet of each module: its #[test]s may grow over the base's only
# by the new ids it represents, the ids whose representative came into it and
# the tests moved into it, less the ids whose representative left it.
allow = dict(moved_in)
for i in new_ids:
    f = file_of(by_id[i]["test"], False)
    if f:
        allow[f] = allow.get(f, 0) + 1
for i in replaced:
    f, g = file_of(by_id[i]["test"], False), file_of(base_reg[i], True)
    if f:
        allow[f] = allow.get(f, 0) + 1
    if g:
        allow[g] = allow.get(g, 0) - 1
count_base, count_work = {}, {}
for name, p in base_tests.items():
    count_base[p] = count_base.get(p, 0) + 1
for name, w in work_tests.items():
    p = w.rsplit(":", 1)[0]
    if p in work_read:
        count_work[p] = count_work.get(p, 0) + 1
over = 0
for p in sorted(base_read | work_read | set(allow)):
    if p not in base_read and p not in work_read:
        grew = 0  # an unchanged file
    else:
        grew = count_work.get(p, 0) - count_base.get(p, 0)
    if grew > allow.get(p, 0):
        print("%s: %s: %+d #[test] over the base, more than its new ids, replaced representatives and moved tests"
              " allow (%+d): a representative replaced but left in place, or a test with no id; remove it, give it an"
              " id or move it to a unit test" % (me, p, grew, allow.get(p, 0)), file=sys.stderr)
        over += 1

if missing:
    print("%s: %d new test(s) in tests/it with no boundary id: move the cases of a decision to a unit test of a"
          " side-effect-free fn in src/, or, when the test keeps a boundary (%s), make it the representative of an id:"
          " replace the representative of an existing id, or add a new id in %s/t<task ID>.toml"
          " (docs/development/testing.md, 判断と境界のtest; the format is in the header of"
          " scripts/check-it-added-tests.sh)" % (me, missing, ", ".join(BOUNDARIES), NEW_DIR), file=sys.stderr)
if missing or bad or over:
    sys.exit(1)
print("%s: %d new test(s) in tests/it since %s, %d new id(s), %d replaced representative(s), each kept: ok"
      % (me, len(new), base[:12], len(new_ids), len(replaced)))
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
