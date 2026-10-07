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
                                            [--full REASON]
       python3 scripts/landing-it.py needs-list --diff FILE --tree FILE
                                                --table FILE|- [--now TIME]
                                                [--table-error TEXT]
       python3 scripts/landing-it.py self-test FIXTURES
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
#    root, "*" within one directory and "**" across any depth. A src or crates
#    .rs file that is not in the tree of the table's commit runs every IT too
#    (the table cannot tell what it runs), see choose().
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
    "tests/common/**",
]
# 3. How old a table may be: older than this since its generated_at, every IT
#    runs. Together with it, the IT not in the table (added or renamed after
#    it was built) always run.
MAX_TABLE_AGE = datetime.timedelta(hours=48)

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


def full_reasons(diff, tree, table, table_error, now, forced=None):
    """The reasons to run every IT that need no list of the tests."""
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
    files = table.get("files", {})
    tree_files = set(tree)
    for path in diff:
        common = [p for p in COMMON_FILES if glob_match(path, p)]
        if common:
            reasons.append("a common file: %s" % path)
        elif (
            is_source(path)
            and path not in files
            and path not in tree_files
            and not is_unit_test_module(path)
        ):
            reasons.append("a source file not in the table's tree: %s" % path)
    return reasons


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


def choose(diff, tree, table, table_error, listing, known, fix_run, threads, now, forced=None):
    """The tests of the landing's verification and why: a dict with full,
    reasons, the IT filtersets (narrowed), the tests not in the table, the
    excluded and kept known failures and the nextest filterset."""
    reasons = full_reasons(diff, tree, table, table_error, now, forced)
    narrowed = []
    from_table = set()
    not_in_table = []
    if not reasons:
        files = table.get("files", {})
        table_tests = table.get("tests", {})
        for path in diff:
            from_table.update(k for k in files.get(path, []) if k in table_tests)
            f = test_file_filter(path)
            if f is not None and f not in narrowed:
                narrowed.append(f)
            for pattern, filters in READ_BY_TESTS:
                if glob_match(path, pattern):
                    narrowed.extend(x for x in filters if x not in narrowed)
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
        args.table_error, now_of(args.now), args.full,
    )
    print("no" if reasons else "yes")


def cmd_select(args):
    choice = choose(
        read_lines(args.diff), read_lines(args.tree), read_json(args.table),
        args.table_error, read_json(args.list), read_json(args.known),
        args.fix_run == "1", threads_of(args.threads), now_of(args.now), args.full,
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
            case.get("full"),
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
