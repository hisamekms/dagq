#!/usr/bin/env python3
"""The steps of scripts/landing-it.sh written in Python: choosing the tests of
the landing's verification from the diff, the nightly IT coverage map and the
known failures, and the self-test. What it chooses and why is in the comment
at the top of scripts/landing-it.sh; run that script, not this file. Python
3.9 and the standard library only (the host's /usr/bin/python3).

Usage: python3 scripts/landing-it.py select --diff FILE --tree FILE
                                            --table FILE|- --list FILE|-
                                            [--known FILE] [--fix-run 0|1]
                                            [--threads N] [--now TIME]
                                            [--table-error TEXT]
                                            [--full REASON] [--root DIR]
       python3 scripts/landing-it.py needs-list --diff FILE --tree FILE
                                                --table FILE|- [--now TIME]
                                                [--table-error TEXT]
                                                [--full REASON] [--root DIR]
       python3 scripts/landing-it.py self-test FIXTURES
--root is the checked-out tree the changed helpers' users are read from
(default: the current directory); the self-test reads FIXTURES/repo.
"""

import argparse
import datetime
import json
import os
import re
import sys

# The values the selection decides by, each from docs/plans/landing-it-selection.md,
# section 「決めたこと（段 3b の ADR と design が使う）」. This is the one place
# they are written; change them there first, then here.
#
# 1. The threshold: run every IT when the narrowed IT's estimated wall time
#    (max(serial sum / NEXTEST_TEST_THREADS, longest test)) exceeds this share
#    of the same estimate for every IT of the table (220 s with the table the
#    measurement used). A share, not seconds: the table's seconds are CI's,
#    measured with coverage.
FULL_SHARE = 0.5
# 2. The common files: touching one runs every IT. Globs from the repository
#    root, "*" within one directory and "**" across any depth.
COMMON_FILES = [
    "Cargo.lock",
    "Cargo.toml",
    "crates/*/Cargo.toml",
    "build.rs",
    "crates/*/build.rs",
    "rust-toolchain.toml",
    ".config/nextest.toml",
    "migrations/**",
    "src/migration_numbers.rs",
]
#    The helpers of the integration tests under tests/common are not common
#    files: a changed helper selects the IT of the modules that use it, read
#    from the checked-out tree (helper_users()). A helper whose users hold
#    more than BROAD_SHARE (below) of the IT by count, mod.rs among them,
#    runs every IT: a broken shared fixture would reach most IT.
# 3. How old a table may be: older than this since its generated_at, every IT
#    runs. Together with it, the IT not in the table (added or renamed after
#    it was built) always run.
MAX_TABLE_AGE = datetime.timedelta(hours=48)
# 4. The broad entries: a file the table maps to more than this share of its
#    IT (by count) selects none of them; the IT through it are left to CI.
#    A src or crates .rs file not in the tree of the table's commit (one the
#    diff added) selects none from the table either. Both still run the
#    diff's own tests and the IT not in the table, see choose(). The same
#    share, by the #[test] count of the tree, makes a helper under
#    tests/common broad (see COMMON_FILES).
BROAD_SHARE = 0.25

# The table's format this reads (scripts/it-coverage-map.sh).
TABLE_VERSION = 1

# The integration tests: the test binaries but the e2e (needs cmux).
IT_FILTER = "kind(test) - binary(e2e)"
# The unit tests, run in full at every landing: the lib and bin targets of
# every crate of the workspace.
UNIT_FILTER = "kind(lib) | kind(bin) | kind(proc-macro)"

# Files the integration tests read from the repository, not through the code
# the table maps (docs/plans/landing-it-selection.md, 「絞った IT の選び方」).
READ_BY_TESTS = [
    ("plugins/**", ["binary_id(=dagq::plugin)", "(binary_id(=dagq::it) & test(/^installed_plugin::/))"]),
    ("scripts/check-migration-numbers.sh", ["(binary_id(=dagq::it) & test(/^queue_schema::/))"]),
    ("dagq.toml", ["(binary_id(=dagq::it) & test(=review_subagents::this_repository_names_only_agents_it_defines))"]),
    (".dagq/agents/**", ["(binary_id(=dagq::it) & test(=review_subagents::this_repository_names_only_agents_it_defines))"]),
]

# The helpers of the integration tests: tests/it/main.rs and tests/plugin.rs
# include tests/common/mod.rs as their module `common`.
HELPER_DIR = "tests/common"


def read_text(root, path):
    """The text of a file without its whole-line comments (doc comments
    name items they do not use), or "" when it cannot be read."""
    try:
        with open(os.path.join(root, path), encoding="utf-8") as f:
            return re.sub(r"(?m)^[ \t]*//.*$", "", f.read())
    except (OSError, UnicodeDecodeError):
        return ""


def rs_files(root, top):
    """The .rs files under root/top, as paths from root, sorted."""
    out = []
    for d, dirs, files in os.walk(os.path.join(root, top)):
        dirs.sort()
        for name in sorted(files):
            if name.endswith(".rs"):
                out.append(os.path.relpath(os.path.join(d, name), root).replace(os.sep, "/"))
    return out


def helper_of(path):
    """The helper a file under tests/common is ("mod" for mod.rs), or None
    when it is not a .rs file of a helper (it runs every IT)."""
    m = re.fullmatch(re.escape(HELPER_DIR) + r"/([^/]+?)(\.rs|/.*\.rs)", path)
    return m.group(1) if m else None


def group_items(text, prefix):
    """The first segments of the items of every `<prefix>::{...}` group in
    text, and whether one of them is a glob."""
    items, glob = set(), re.search(r"\b%s::\*" % prefix, text) is not None
    for m in re.finditer(r"\b%s::\{" % prefix, text):
        depth, start, i = 1, m.end(), m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        part, item, depth = text[start:i - 1], "", 0
        for ch in part + ",":
            if ch == "," and depth == 0:
                first = re.split(r"::|\s+as\s+", item.strip())[0].strip()
                if first == "*":
                    glob = True
                elif first:
                    items.add(first)
                item = ""
                continue
            depth += {"{": 1, "}": -1}.get(ch, 0)
            item += ch
    return items, glob


def names_helpers(text, helpers, reexported, prefixes):
    """The helpers a file's text names through one of prefixes (`common` from
    tests/it and tests/plugin.rs, `super` and `common` from another helper),
    directly, in a use group or by a name mod.rs re-exports."""
    out = set()
    for prefix in prefixes:
        items, glob = group_items(text, prefix)
        if glob:
            return set(helpers)
        for h in helpers:
            if h in items or re.search(r"\b%s::%s\b" % (prefix, h), text):
                out.add(h)
    for name, h in reexported.items():
        if re.search(r"\b%s\b" % re.escape(name), text):
            out.add(h)
    return out


def test_tree(root):
    """What the choice of a changed helper reads from the tree at root, read
    once: the helpers and which helpers each names (mod.rs only by code
    beyond its mod and pub use lines), the names mod.rs re-exports, and for
    each test module ("it::<top module>" of tests/it, and "plugin" for
    tests/plugin.rs) the helpers it names, the modules it names
    (`crate::<m>`, `super::<m>`, or `<m>` in a `crate::{...}` group, which a
    bare `<m>::` in a module needs first), the macros it defines and calls, and its
    #[test] count. total is the #[test] count of every IT file (tests/it,
    tests/plugin.rs, crates/*/tests)."""
    files = rs_files(root, HELPER_DIR)
    helpers = sorted({helper_of(p) for p in files} - {None, "mod"})
    mod_text = read_text(root, HELPER_DIR + "/mod.rs")
    reexported = {}
    for m in re.finditer(r"pub\s+use\s+(?:self::)?(\w+)::([^;]+);", mod_text):
        if m.group(1) in helpers:
            for name in re.findall(r"\b([A-Za-z_]\w*)\b", m.group(2)):
                if name not in ("self", "crate", "super"):
                    reexported[name] = m.group(1)
    deps = {}
    for h in helpers:
        text = "\n".join(read_text(root, p) for p in files if helper_of(p) == h)
        deps[h] = names_helpers(text, helpers, reexported, ("super", "common")) - {h}
    body = re.sub(r"pub\s+(mod\s+\w+|use\s+[^;]+);", "", mod_text)
    deps["mod"] = {h for h in helpers if re.search(r"\b%s::" % h, body)}
    paths = {}
    for path in rs_files(root, "tests/it"):
        top = path[len("tests/it/"):].split("/")[0]
        top = top[:-3] if top.endswith(".rs") else top
        if top != "main":
            paths.setdefault("it::" + top, []).append(path)
    paths["plugin"] = ["tests/plugin.rs"]
    modules = {}
    for key, ps in paths.items():
        text = "\n".join(read_text(root, p) for p in ps)
        names = names_helpers(text, helpers, reexported, ("common",))
        if re.search(r"\bcommon\b", text):
            names.add("mod")
        modules[key] = {
            "helpers": names,
            "refs": set(re.findall(r"\b(?:crate|super)::(\w+)", text))
            | group_items(text, "crate")[0],
            "defines": set(re.findall(r"macro_rules!\s*(\w+)", text)),
            "calls": set(re.findall(r"\b(\w+)!", text)),
            "tests": len(re.findall(r"#\[test\]", text)),
        }
    every = [p for ps in paths.values() for p in ps]
    crates = os.path.join(root, "crates")
    for c in sorted(os.listdir(crates)) if os.path.isdir(crates) else []:
        every.extend(rs_files(root, "crates/%s/tests" % c))
    total = sum(len(re.findall(r"#\[test\]", read_text(root, p))) for p in every)
    return {"helpers": helpers, "deps": deps, "modules": modules, "total": total}


def helper_users(tree, changed):
    """The test modules that use the changed helpers (names as helper_of
    gives them): through another helper that names one (mod.rs's own code
    included, which every module naming `common` reaches), and a tests/it
    module through another one it names or whose macros it calls."""
    reached = set(changed)
    while True:
        more = {h for h, d in tree["deps"].items() if d & reached} - reached
        if not more:
            break
        reached |= more
    modules = tree["modules"]
    users = {k for k, m in modules.items() if m["helpers"] & reached}
    while True:
        names = {k[len("it::"):] for k in users if k.startswith("it::")}
        macros = set().union(*(modules[k]["defines"] for k in users)) if users else set()
        more = {
            k for k, m in modules.items()
            if k not in users and k.startswith("it::")
            and (m["refs"] & names or m["calls"] & macros)
        }
        if not more:
            break
        users |= more
    return users


def module_filter(key):
    if key == "plugin":
        return "binary_id(=dagq::plugin)"
    return "(binary_id(=dagq::it) & test(/^%s::/))" % key[len("it::"):]


def helper_choice(root, diff):
    """For the diff's files under tests/common: the reasons to run every IT
    (a file that is not a helper's .rs, a helper whose users hold more than
    BROAD_SHARE of the IT by #[test] count) and otherwise the filtersets of
    the modules that use the changed helpers."""
    paths = [p for p in diff if p.startswith(HELPER_DIR + "/")]
    if not paths:
        return [], []
    tree = test_tree(root)
    limit = BROAD_SHARE * tree["total"]
    reasons, filters = [], []
    for p in paths:
        h = helper_of(p)
        if h is None:
            reasons.append("a file under %s that is no helper: %s" % (HELPER_DIR, p))
            continue
        users = helper_users(tree, {h})
        n = sum(tree["modules"][k]["tests"] for k in users)
        if n > limit:
            reasons.append("a helper whose users hold %d of %d IT: %s" % (n, tree["total"], p))
        else:
            filters.extend(f for f in map(module_filter, sorted(users)) if f not in filters)
    return reasons, filters


def glob_match(path, pattern):
    """Whether path matches pattern: "*" stays in one directory, "**" spans
    any depth."""
    regex = ""
    i = 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            regex += ".*"
            i += 2
        elif pattern[i] == "*":
            regex += "[^/]*"
            i += 1
        else:
            regex += re.escape(pattern[i])
            i += 1
    return re.fullmatch(regex, path) is not None


def parse_time(text):
    return datetime.datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=datetime.timezone.utc
    )


def test_file_filter(path):
    """The filterset of the tests a test file defines, or None when path is
    not a test file of an integration test binary."""
    m = re.fullmatch(r"tests/it/(.+)\.rs", path)
    if m:
        parts = m.group(1).split("/")
        if parts[-1] == "mod":
            parts = parts[:-1]
        if parts == ["main"] or not parts:
            return None
        return "(binary_id(=dagq::it) & test(/^%s::/))" % "::".join(parts)
    if path == "tests/plugin.rs":
        return "binary_id(=dagq::plugin)"
    m = re.fullmatch(r"crates/([^/]+)/tests/([^/]+)\.rs", path)
    if m:
        return "binary_id(=%s::%s)" % (m.group(1), m.group(2))
    return None


def is_unit_test_module(path):
    """A src or crates .rs file of unit tests only: they run in full anyway."""
    name = os.path.basename(path)
    inner = re.sub(r"^(crates/[^/]+/)?src/", "", path)
    return name == "tests.rs" or name.endswith("_tests.rs") or "tests" in inner.split("/")[:-1]


def is_source(path):
    return path.endswith(".rs") and (
        path.startswith("src/") or re.match(r"crates/[^/]+/src/", path) is not None
    )


def wall(durations, threads):
    """The estimated wall time of tests with these durations."""
    threads = max(threads, 1)
    if not durations:
        return 0.0
    return max(sum(durations) / threads, max(durations))


def filter_quote(name):
    """A test name as the argument of test(=...)."""
    return name.replace("\\", "\\\\").replace(")", "\\)").replace(",", "\\,")


def one_test(binary, name):
    """The filterset of one test by its binary id and name."""
    return "(binary_id(=%s) & test(=%s))" % (binary, filter_quote(name))


def full_reasons(diff, tree, table, table_error, now, forced=None, root="."):
    """The reasons to run every IT that need no list of the tests; root is
    the checked-out tree the changed helpers' users are read from."""
    reasons = [forced] if forced else []
    if table is None:
        reasons.append("the table is not available: %s" % (table_error or "unknown"))
        return reasons
    try:
        if table.get("version") != TABLE_VERSION:
            raise ValueError("version %r" % table.get("version"))
        generated = parse_time(table["generated_at"])
    except (KeyError, TypeError, ValueError, AttributeError) as e:
        reasons.append("the table cannot be read: %s" % e)
        return reasons
    if now - generated > MAX_TABLE_AGE:
        reasons.append(
            "the table is older than %d hours: generated_at %s"
            % (MAX_TABLE_AGE.total_seconds() // 3600, table["generated_at"])
        )
    for path in diff:
        if any(glob_match(path, p) for p in COMMON_FILES):
            reasons.append("a common file: %s" % path)
    reasons.extend(helper_choice(root, diff)[0])
    return reasons


def left_to_ci(diff, tree, table):
    """The diff's files whose IT the table does not select, each with why:
    the broad entries and the source files not in the table's tree."""
    files = table.get("files", {})
    limit = BROAD_SHARE * len(table.get("tests", {}))
    tree_files = set(tree)
    out = []
    for path in diff:
        if path in files and len(files[path]) > limit:
            out.append("%s (the table maps it to %d of %d IT)"
                       % (path, len(files[path]), len(table.get("tests", {}))))
        elif (
            is_source(path)
            and path not in files
            and path not in tree_files
            and not is_unit_test_module(path)
        ):
            out.append("%s (not in the table's tree)" % path)
    return out


def known_failures(known):
    """The filtersets to leave out (failures) and the names kept for a fix
    run (kept_for_task), from the JSON of dagq ci failures."""
    excluded, kept = [], []
    if known is None:
        return excluded, kept
    for key, out in (("failures", excluded), ("kept_for_task", kept)):
        for item in known.get(key) or []:
            if item.get("kind") != "test":
                continue
            name = item.get("name", "")
            binary, sep, test = name.partition(" ")
            if sep:
                out.append((name, "(binary_id(=%s) & test(=%s))" % (binary, filter_quote(test))))
            else:
                out.append((name, "test(=%s)" % filter_quote(name)))
    return excluded, kept


def current_its(listing):
    """The integration tests nextest lists now: key -> (binary id, name)."""
    out = {}
    for binary, suite in listing.get("rust-suites", {}).items():
        if suite.get("kind") != "test":
            continue
        for name, case in suite.get("testcases", {}).items():
            if case.get("filter-match", {}).get("status") != "matches":
                continue
            if case.get("ignored"):
                continue
            out["%s::%s" % (binary, name)] = (binary, name)
    return out


def choose(diff, tree, table, table_error, listing, known, fix_run, threads, now, forced=None,
           root="."):
    """The tests of the landing's verification and why: a dict with full,
    reasons, the IT filtersets (narrowed), the tests not in the table, the
    excluded and kept known failures and the nextest filterset."""
    reasons = full_reasons(diff, tree, table, table_error, now, forced, root)
    narrowed = []
    from_table = set()
    not_in_table = []
    left = []
    if not reasons:
        files = table.get("files", {})
        table_tests = table.get("tests", {})
        limit = BROAD_SHARE * len(table_tests)
        for path in diff:
            if len(files.get(path, [])) <= limit:
                from_table.update(k for k in files.get(path, []) if k in table_tests)
            f = test_file_filter(path)
            if f is not None and f not in narrowed:
                narrowed.append(f)
            for pattern, filters in READ_BY_TESTS:
                if glob_match(path, pattern):
                    narrowed.extend(x for x in filters if x not in narrowed)
        narrowed.extend(f for f in helper_choice(root, diff)[1] if f not in narrowed)
        all_durations = [t.get("duration_secs") or 0.0 for t in table_tests.values()]
        selected = [table_tests.get(k, {}).get("duration_secs") or 0.0 for k in from_table]
        limit = FULL_SHARE * wall(all_durations, threads)
        estimate = wall(selected, threads)
        if estimate > limit:
            reasons.append(
                "the narrowed IT's estimated wall time %.1f s exceeds %.1f s (%g of every IT's)"
                % (estimate, limit, FULL_SHARE)
            )
        else:
            left = left_to_ci(diff, tree, table)
            listed = current_its(listing or {})
            not_in_table = sorted(k for k in listed if k not in table_tests)
            for key in sorted(from_table):
                test = table_tests.get(key, {})
                narrowed.append(one_test(test.get("binary_id"), test.get("name")))
            narrowed.extend(one_test(*listed[k]) for k in not_in_table)
    full = bool(reasons)
    excluded, kept = known_failures(known)
    if not full:
        # A fix run runs the tests of its findings whatever the diff reaches.
        narrowed.extend(f for _, f in kept if f not in narrowed)
    if full:
        it = "(%s)" % IT_FILTER
    elif narrowed:
        it = "(%s)" % " | ".join(narrowed)
    else:
        it = None
    expr = "(%s)" % UNIT_FILTER
    if it:
        expr += " | " + it
    if excluded:
        expr = "(%s) - (%s)" % (expr, " | ".join(f for _, f in excluded))
    return {
        "full": full,
        "reasons": reasons,
        "narrowed": [] if full else narrowed,
        "not_in_table": not_in_table,
        "left_to_ci": left,
        "excluded": [n for n, _ in excluded],
        "kept": [n for n, _ in kept],
        "fix_run": fix_run,
        "filter": expr,
    }


def report(choice):
    """The lines landing-it.sh prints before it runs the tests."""
    lines = []
    if choice["full"]:
        lines.append("landing-it: every IT runs:")
        lines.extend("landing-it:   %s" % r for r in choice["reasons"])
    else:
        lines.append(
            "landing-it: narrowed IT: %d filters (%d not in the table)"
            % (len(choice["narrowed"]), len(choice["not_in_table"]))
        )
        lines.extend("landing-it:   %s" % f for f in choice["narrowed"])
        lines.extend("landing-it: its IT left to CI: %s" % f for f in choice["left_to_ci"])
    for name in choice["excluded"]:
        lines.append("landing-it: left out, fails on main already: %s" % name)
    for name in choice["kept"]:
        lines.append("landing-it: kept, this run fixes it: %s" % name)
    if choice["fix_run"] and not choice["kept"]:
        lines.append("landing-it: a CI fix run with no test of its findings in the list")
    return "\n".join(lines)


def read_lines(path):
    with open(path, encoding="utf-8") as f:
        return [line for line in f.read().splitlines() if line]


def read_json(path):
    if path in (None, "", "-"):
        return None
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def now_of(text):
    if text:
        return parse_time(text)
    return datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)


def threads_of(text):
    """NEXTEST_TEST_THREADS as a number; num-cpus (or anything else) is the
    host's CPU count."""
    try:
        return max(int(text), 1)
    except ValueError:
        return os.cpu_count() or 1


def cmd_needs_list(args):
    reasons = full_reasons(
        read_lines(args.diff), read_lines(args.tree), read_json(args.table),
        args.table_error, now_of(args.now), args.full, args.root,
    )
    print("no" if reasons else "yes")


def cmd_select(args):
    choice = choose(
        read_lines(args.diff), read_lines(args.tree), read_json(args.table),
        args.table_error, read_json(args.list), read_json(args.known),
        args.fix_run == "1", threads_of(args.threads), now_of(args.now), args.full, args.root,
    )
    print(report(choice), file=sys.stderr)
    print(choice["filter"])


def cmd_self_test(args):
    with open(os.path.join(args.fixtures, "cases.json"), encoding="utf-8") as f:
        cases = json.load(f)
    table = read_json(os.path.join(args.fixtures, "table.json"))
    listing = read_json(os.path.join(args.fixtures, "list.json"))
    tree = read_lines(os.path.join(args.fixtures, "tree.txt"))
    failed = 0
    for case in cases:
        case_table = None if case.get("no_table") else dict(table, **case.get("table_patch", {}))
        choice = choose(
            case["diff"], tree, case_table,
            "gh failed" if case.get("no_table") else None, listing,
            case.get("known"), case.get("fix_run", False), 6, parse_time(case["now"]),
            case.get("full"), os.path.join(args.fixtures, "repo"),
        )
        want = case["want"]
        got = {k: choice[k] for k in want}
        if got != want:
            failed += 1
            print("FAIL %s:\n  want %s\n  got  %s" % (
                case["name"], json.dumps(want, sort_keys=True), json.dumps(got, sort_keys=True)),
                file=sys.stderr)
        else:
            print("ok   %s" % case["name"])
    if failed:
        print("landing-it self-test: %d of %d cases failed" % (failed, len(cases)), file=sys.stderr)
        sys.exit(1)
    print("landing-it self-test: %d cases passed" % len(cases))


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name in ("select", "needs-list"):
        p = sub.add_parser(name)
        p.add_argument("--diff", required=True)
        p.add_argument("--tree", required=True)
        p.add_argument("--table", required=True)
        p.add_argument("--table-error")
        p.add_argument("--now")
        p.add_argument("--full", help="run every IT for this reason")
        p.add_argument("--root", default=".", help="the tree the helpers' users are read from")
        if name == "select":
            p.add_argument("--list", required=True)
            p.add_argument("--known")
            p.add_argument("--fix-run", default="0")
            p.add_argument("--threads", default="6")
    p = sub.add_parser("self-test")
    p.add_argument("fixtures")
    args = parser.parse_args()
    {"select": cmd_select, "needs-list": cmd_needs_list, "self-test": cmd_self_test}[args.cmd](args)


if __name__ == "__main__":
    main()
