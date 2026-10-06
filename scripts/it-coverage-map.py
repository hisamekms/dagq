#!/usr/bin/env python3
"""The steps of scripts/it-coverage-map.sh written in Python: the per-test
llvm-cov export, assembling the file -> test map, and the self-test. The table
format, the test names and the artifact are in the comment at the top of
scripts/it-coverage-map.sh; run that script, not this file. Python 3.9 and the
standard library only (the host's /usr/bin/python3).

Usage: python3 scripts/it-coverage-map.py export --tests-dir DIR --list LIST
                                                 [--jobs N]
       python3 scripts/it-coverage-map.py assemble --tests-dir DIR --list LIST
                                                   --root ROOT --commit SHA
                                                   --out OUT [--junit FILE...]
                                                   [--generated-at TIME]
                                                   [--run-url URL]
       python3 scripts/it-coverage-map.py self-test FIXTURES
"""

import argparse
import concurrent.futures
import datetime
import glob
import json
import os
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

FORMAT_VERSION = 1
FILTER = "kind(test) - binary(e2e)"
# The sources outside the repository llvm-cov export leaves out (the
# registry's and git dependencies' crates, the standard library).
IGNORE_FILENAME_REGEX = r"/(\.cargo/(registry|git)|\.rustup|rustc)/"


def read_test_dir(path):
    """The binary id and the test name the runner wrote, or None."""
    try:
        with open(os.path.join(path, "test.txt"), encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return None
    if len(lines) < 2 or not lines[0] or not lines[1]:
        return None
    return lines[0], lines[1]


def test_dirs(tests_dir):
    """Every <binary id>/<test name> directory the runner made."""
    out = []
    for path in sorted(glob.glob(os.path.join(tests_dir, "*", "*"))):
        ident = read_test_dir(path)
        if ident is not None:
            out.append((path, ident[0], ident[1]))
    return out


def non_test_binaries(listing):
    """The absolute paths of the binaries the tests spawn (the dagq CLI and the
    broker's binaries), from cargo nextest list's rust-build-meta."""
    meta = listing.get("rust-build-meta", {})
    target = meta.get("target-directory", "")
    out = []
    for binaries in meta.get("non-test-binaries", {}).values():
        for b in binaries:
            if b.get("kind") != "bin-exe":
                continue
            path = b.get("path", "")
            if not os.path.isabs(path):
                path = os.path.join(target, path)
            out.append(path)
    return sorted(set(out))


def llvm_tool(name):
    env = os.environ.get(name.upper().replace("-", "_"))
    if env:
        return env
    sysroot = subprocess.run(
        ["rustc", "--print", "sysroot"], check=True, capture_output=True, text=True
    ).stdout.strip()
    host = ""
    for line in subprocess.run(
        ["rustc", "-vV"], check=True, capture_output=True, text=True
    ).stdout.splitlines():
        if line.startswith("host: "):
            host = line[len("host: "):]
    return os.path.join(sysroot, "lib", "rustlib", host, "bin", name)


def export_one(path, objects, profdata, cov):
    """Merge one test's profraw files and write its llvm-cov export
    (summary only) to export.json; the profraw files are removed after."""
    profraws = sorted(glob.glob(os.path.join(path, "*.profraw")))
    if not profraws:
        return "no profraw"
    try:
        with open(os.path.join(path, "binary.txt"), encoding="utf-8") as f:
            binary = f.read().strip()
    except OSError:
        return "no binary.txt"
    merged = os.path.join(path, "test.profdata")
    r = subprocess.run(
        [profdata, "merge", "-sparse", "-o", merged] + profraws,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        return "llvm-profdata merge: " + r.stderr.strip()[:500]
    args = [cov, "export", "-summary-only", "-instr-profile", merged,
            "-ignore-filename-regex", IGNORE_FILENAME_REGEX, binary]
    for o in objects:
        if o != binary and os.path.exists(o):
            args += ["-object", o]
    tmp = os.path.join(path, "export.json.tmp")
    with open(tmp, "w", encoding="utf-8") as out:
        r = subprocess.run(args, stdout=out, stderr=subprocess.PIPE, text=True)
    if r.returncode != 0:
        os.remove(tmp)
        return "llvm-cov export: " + r.stderr.strip()[:500]
    os.replace(tmp, os.path.join(path, "export.json"))
    for p in profraws + [merged]:
        os.remove(p)
    return None


def cmd_export(a):
    with open(a.list, encoding="utf-8") as f:
        listing = json.load(f)
    objects = non_test_binaries(listing)
    profdata = llvm_tool("llvm-profdata")
    cov = llvm_tool("llvm-cov")
    # A test exported (or failed to export) by an earlier partition is left.
    dirs = [d for d in test_dirs(a.tests_dir)
            if not os.path.exists(os.path.join(d[0], "export.json"))
            and not os.path.exists(os.path.join(d[0], "export.err"))]
    errors = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.jobs) as pool:
        futures = {pool.submit(export_one, d[0], objects, profdata, cov): d
                   for d in dirs}
        for fut in concurrent.futures.as_completed(futures):
            err = fut.result()
            if err:
                errors += 1
                d = futures[fut]
                with open(os.path.join(d[0], "export.err"), "w",
                          encoding="utf-8") as f:
                    f.write(err + "\n")
                print("it-coverage-map: %s %s: %s" % (d[1], d[2], err),
                      file=sys.stderr)
    print("it-coverage-map: exported %d tests (%d without an export)"
          % (len(dirs) - errors, errors), file=sys.stderr)
    return 0


def covered_files(export, root):
    """The repository files (paths relative to root, / separated) whose
    lines the export counts as covered at least once. target/ (generated
    sources) and files outside root are left out."""
    root = os.path.realpath(root)
    out = set()
    for data in export.get("data", []):
        for f in data.get("files", []):
            lines = f.get("summary", {}).get("lines", {})
            if lines.get("covered", 0) <= 0:
                continue
            name = os.path.realpath(f.get("filename", ""))
            if not name.startswith(root + os.sep):
                continue
            rel = os.path.relpath(name, root).replace(os.sep, "/")
            if rel.startswith("target/"):
                continue
            out.add(rel)
    return out


def junit_results(paths):
    """{(binary id, test name): (seconds, status)} from nextest's JUnit
    files: a testsuite per binary id, a testcase per test. status is passed,
    failed (a failure or error) or flaky (failed, then passed on a retry:
    flakyFailure children; under flaky-result = "fail" nextest also writes a
    failure of type "flaky failure", which does not make it failed). A later
    file wins for the same test."""
    out = {}
    for path in paths:
        if not os.path.exists(path):
            continue
        tree = ET.parse(path)
        for suite in tree.getroot().iter("testsuite"):
            binary = suite.get("name", "")
            for case in suite.iter("testcase"):
                tags = {child.tag for child in case}
                if "skipped" in tags:
                    continue
                real = [c for c in case if c.tag in ("failure", "error")
                        and c.get("type") != "flaky failure"]
                if real:
                    status = "failed"
                elif tags & {"flakyFailure", "flakyError"}:
                    status = "flaky"
                else:
                    status = "passed"
                try:
                    secs = round(float(case.get("time", "")), 3)
                except ValueError:
                    secs = None
                out[(binary, case.get("name", ""))] = (secs, status)
    return out


def listed_tests(listing):
    """The (binary id, test name) pairs the filter selected (not ignored)."""
    out = set()
    for binary, suite in listing.get("rust-suites", {}).items():
        for name, case in suite.get("testcases", {}).items():
            if case.get("ignored"):
                continue
            if case.get("filter-match", {}).get("status") != "matches":
                continue
            out.add((binary, name))
    return out


def assemble(tests_dir, listing, junit, root, commit, generated_at, run_url):
    exports = {}
    for path, binary, name in test_dirs(tests_dir):
        e = os.path.join(path, "export.json")
        if os.path.exists(e):
            with open(e, encoding="utf-8") as f:
                exports[(binary, name)] = covered_files(json.load(f), root)
    results = junit_results(junit)
    keys = listed_tests(listing) | set(results) | set(exports)
    tests = {}
    files = {}
    for binary, name in sorted(keys):
        key = binary + "::" + name
        secs, status = results.get((binary, name), (None, "not_run"))
        covered = exports.get((binary, name))
        tests[key] = {
            "binary_id": binary,
            "name": name,
            "duration_secs": secs,
            "status": status,
            "coverage": covered is not None,
            "files": len(covered or ()),
        }
        for rel in covered or ():
            files.setdefault(rel, []).append(key)
    return {
        "version": FORMAT_VERSION,
        "commit": commit,
        "generated_at": generated_at,
        "run_url": run_url,
        "filter": FILTER,
        "tests": tests,
        "files": {k: sorted(v) for k, v in sorted(files.items())},
    }


def cmd_assemble(a):
    with open(a.list, encoding="utf-8") as f:
        listing = json.load(f)
    generated_at = a.generated_at or datetime.datetime.now(
        datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    table = assemble(a.tests_dir, listing, a.junit or [], a.root, a.commit,
                     generated_at, a.run_url)
    tmp = a.out + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(table, f, indent=1, sort_keys=True)
        f.write("\n")
    os.replace(tmp, a.out)
    no_cov = sum(1 for t in table["tests"].values() if not t["coverage"])
    if table["tests"] and no_cov == len(table["tests"]):
        # A build without coverage or a runner that never ran: the table
        # would map no file, so fail rather than upload it.
        print("it-coverage-map: no test has coverage (%d tests); the build "
              "or the runner is broken" % no_cov, file=sys.stderr)
        return 1
    print("it-coverage-map: %d tests, %d files, %d tests without coverage -> %s"
          % (len(table["tests"]), len(table["files"]), no_cov, a.out),
          file=sys.stderr)
    return 0


def cmd_self_test(a):
    fx = a.fixtures
    with open(os.path.join(fx, "list.json"), encoding="utf-8") as f:
        listing = json.load(f)
    # The fixtures' exports name files under /work/dagq; root is that path
    # whether or not it exists (realpath leaves a missing path as it is).
    got = assemble(os.path.join(fx, "tests"), listing,
                   [os.path.join(fx, "junit.xml")], "/work/dagq",
                   "0123456789abcdef0123456789abcdef01234567",
                   "2026-10-06T18:00:00Z", None)
    with open(os.path.join(fx, "expected.json"), encoding="utf-8") as f:
        want = json.load(f)
    if got != want:
        print("it-coverage-map: self-test: the table differs from expected.json",
              file=sys.stderr)
        print(json.dumps(got, indent=1, sort_keys=True), file=sys.stderr)
        return 1
    # The objects of the export: the binaries the tests spawn, made absolute.
    if non_test_binaries(listing) != ["/work/dagq/target/debug/dagq"]:
        print("it-coverage-map: self-test: non_test_binaries: %r"
              % non_test_binaries(listing), file=sys.stderr)
        return 1
    # The assembled file round-trips through cmd_assemble's writer.
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "map.json")
        ns = argparse.Namespace(
            tests_dir=os.path.join(fx, "tests"), list=os.path.join(fx, "list.json"),
            junit=[os.path.join(fx, "junit.xml")], root="/work/dagq",
            commit="0123456789abcdef0123456789abcdef01234567",
            generated_at="2026-10-06T18:00:00Z", run_url=None, out=out)
        cmd_assemble(ns)
        with open(out, encoding="utf-8") as f:
            if json.load(f) != want:
                print("it-coverage-map: self-test: the written table differs",
                      file=sys.stderr)
                return 1
    return 0


def main(argv):
    p = argparse.ArgumentParser(prog="it-coverage-map.py")
    sub = p.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("export")
    e.add_argument("--tests-dir", required=True)
    e.add_argument("--list", required=True)
    e.add_argument("--jobs", type=int, default=os.cpu_count() or 2)
    s = sub.add_parser("assemble")
    s.add_argument("--tests-dir", required=True)
    s.add_argument("--list", required=True)
    s.add_argument("--junit", action="append")
    s.add_argument("--root", required=True)
    s.add_argument("--commit", required=True)
    s.add_argument("--out", required=True)
    s.add_argument("--generated-at")
    s.add_argument("--run-url")
    t = sub.add_parser("self-test")
    t.add_argument("fixtures")
    a = p.parse_args(argv)
    return {"export": cmd_export, "assemble": cmd_assemble,
            "self-test": cmd_self_test}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
