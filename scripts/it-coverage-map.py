#!/usr/bin/env python3
"""The steps of scripts/it-coverage-map.sh written in Python: the per-test
llvm-cov export, assembling the file -> test map (at once or as parts of
partitions that are merged), and the self-test. The table and part formats, the test names and the artifact are in the comment at the top of
scripts/it-coverage-map.sh; run that script, not this file. Python 3.9 and the
standard library only (the host's /usr/bin/python3).

Usage: python3 scripts/it-coverage-map.py export --tests-dir DIR --list LIST
                                                 [--jobs N]
       python3 scripts/it-coverage-map.py assemble --tests-dir DIR --list LIST
                                                   --root ROOT --commit SHA
                                                   --out OUT [--junit FILE...]
                                                   [--generated-at TIME]
                                                   [--run-url URL]
       python3 scripts/it-coverage-map.py part --tests-dir DIR --list LIST
                                               --junit FILE --root ROOT
                                               --commit SHA --index K
                                               --count N --out OUT
       python3 scripts/it-coverage-map.py merge --parts-dir DIR --count N
                                                --root ROOT --commit SHA
                                                --out OUT [--generated-at TIME]
                                                [--run-url URL]
       python3 scripts/it-coverage-map.py self-test FIXTURES
"""

import argparse
import concurrent.futures
import datetime
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

FORMAT_VERSION = 1
PART_VERSION = 1
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


def platform_name():
    """The table's name of the OS this runs on."""
    return {"darwin": "macos"}.get(sys.platform, sys.platform)


# What a Rust source says outside comments and string literals; a literal is
# blanked except "apple", which the condition below reads.
RUST_NOISE = re.compile(
    r"""//[^\n]*|/\*.*?\*/|r(?P<hashes>#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|'(?:\\.[^']*|[^'\\])'""",
    re.DOTALL,
)
RUST_CFG = re.compile(r"#!?\[\s*cfg\s*\(|\bcfg!\s*\(")
PLATFORM_CONDITION = re.compile(r'\btarget_os\b|\btarget_vendor\s*=\s*"apple"')


def has_platform_condition(source):
    """Whether a #[cfg(...)], #![cfg(...)] or cfg!(...) of this Rust source
    names target_os or target_vendor = "apple"."""
    def blank(match):
        text = match.group(0)
        if text == '"apple"':
            return text
        return '""' if text.endswith('"') else " "

    code = RUST_NOISE.sub(blank, source)
    for match in RUST_CFG.finditer(code):
        depth = 1
        end = match.end()
        while end < len(code) and depth:
            depth += {"(": 1, ")": -1}.get(code[end], 0)
            end += 1
        if PLATFORM_CONDITION.search(code[match.end():end]):
            return True
    return False


def git(root, *args):
    return subprocess.run(["git", "-C", root] + list(args), capture_output=True)


def platform_gated_files(root, commit):
    """The .rs files under src/ and crates/ in the commit's tree with an OS
    condition, sorted. git grep narrows the files the check reads."""
    r = git(root, "grep", "-lzE", "target_os|target_vendor", commit, "--",
            "src/*.rs", "crates/*.rs")
    if r.returncode not in (0, 1):
        raise RuntimeError("git grep: " + r.stderr.decode("utf-8", "replace").strip())
    out = []
    for item in r.stdout.decode("utf-8", "replace").split("\0"):
        path = item[len(commit) + 1:]
        if not path:
            continue
        shown = git(root, "show", commit + ":" + path)
        if shown.returncode != 0:
            raise RuntimeError("git show: " + shown.stderr.decode("utf-8", "replace").strip())
        if has_platform_condition(shown.stdout.decode("utf-8", "replace")):
            out.append(path)
    return sorted(out)


def read_exports(tests_dir, root):
    """{(binary id, test name): covered files} of the exports under tests_dir."""
    exports = {}
    for path, binary, name in test_dirs(tests_dir):
        e = os.path.join(path, "export.json")
        if os.path.exists(e):
            with open(e, encoding="utf-8") as f:
                exports[(binary, name)] = covered_files(json.load(f), root)
    return exports


def build_table(listed, results, exports, commit, generated_at, run_url,
                platform, gated):
    keys = set(listed) | set(results) | set(exports)
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
        "platform": platform,
        "platform_gated_files": list(gated),
        "tests": tests,
        "files": {k: sorted(v) for k, v in sorted(files.items())},
    }


def assemble(tests_dir, listing, junit, root, commit, generated_at, run_url,
             platform, gated):
    return build_table(listed_tests(listing), junit_results(junit),
                       read_exports(tests_dir, root), commit, generated_at,
                       run_url, platform, gated)


def make_part(tests_dir, listing, junit, root, commit, index, count, platform):
    """One partition's part: the table's material with no path of the machine,
    so that the merge needs neither the build nor the test binaries."""
    results = junit_results(junit)
    exports = read_exports(tests_dir, root)
    return {
        "version": PART_VERSION,
        "commit": commit,
        "platform": platform,
        "partition": {"index": index, "count": count},
        "listed": [list(k) for k in sorted(listed_tests(listing))],
        "results": [[k[0], k[1], v[0], v[1]] for k, v in sorted(results.items())],
        "covered": [[k[0], k[1], sorted(v)] for k, v in sorted(exports.items())],
    }


def some(pairs):
    """A short list of tests for an error message."""
    names = sorted(b + "::" + n for b, n in pairs)
    return ", ".join(names[:10]) + (" and %d more" % (len(names) - 10)
                                    if len(names) > 10 else "")


def merge_parts(parts, count, commit):
    """(problems, listed, results, exports, platform) of the parts. Any problem
    means the table would lack something or mix runs, so no table is made.
    Failed and flaky tests are no problem: their status goes in the table."""
    problems = []
    seen = set()
    listed = None
    platforms = set()
    results = {}
    exports = {}
    for part in parts:
        index = part.get("partition", {}).get("index")
        if part.get("version") != PART_VERSION:
            problems.append("part %s: version %r" % (index, part.get("version")))
            continue
        if part.get("partition", {}).get("count") != count:
            problems.append("part %s: of %s partitions, not %d"
                            % (index, part.get("partition", {}).get("count"), count))
        if index in seen:
            problems.append("part %s: more than once" % index)
        seen.add(index)
        if part.get("commit") != commit:
            problems.append("part %s: commit %s, not %s"
                            % (index, part.get("commit"), commit))
        platforms.add(part.get("platform"))
        # Every export of the job failed (a full disk): as good as no part.
        if part.get("results") and not part.get("covered"):
            problems.append("part %s: no test has coverage" % index)
        this_listed = {tuple(t) for t in part.get("listed", [])}
        if listed is None:
            listed = this_listed
        elif this_listed != listed:
            problems.append("part %s: another list of tests (differ: %s)"
                            % (index, some(this_listed ^ listed)))
        for binary, name, secs, status in part.get("results", []):
            if (binary, name) in results:
                problems.append("part %s: %s::%s is in another part too"
                                % (index, binary, name))
            results[(binary, name)] = (secs, status)
        for binary, name, files in part.get("covered", []):
            exports[(binary, name)] = set(files)
    missing = sorted(set(range(1, count + 1)) - seen)
    if missing:
        problems.append("missing parts: %s of %d" % (missing, count))
    if len(platforms) > 1:
        problems.append("parts of different OSes: %s" % sorted(map(str, platforms)))
    listed = listed or set()
    # With a part missing its tests are expectedly absent: that is the problem.
    if not missing:
        not_run = listed - set(results)
        if not_run:
            problems.append("%d listed tests did not run: %s"
                            % (len(not_run), some(not_run)))
    unlisted = (set(results) | set(exports)) - listed
    if unlisted:
        problems.append("%d tests ran but are not listed: %s"
                        % (len(unlisted), some(unlisted)))
    platform = next(iter(platforms)) if len(platforms) == 1 else None
    return problems, listed, results, exports, platform


def write_json(value, out):
    tmp = out + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(value, f, indent=1, sort_keys=True)
        f.write("\n")
    os.replace(tmp, out)


def write_table(table, out):
    """Write the table unless it maps nothing; the exit status."""
    no_cov = sum(1 for t in table["tests"].values() if not t["coverage"])
    if table["tests"] and no_cov == len(table["tests"]):
        # A build without coverage or a runner that never ran: the table
        # would map no file, so fail rather than upload it.
        print("it-coverage-map: no test has coverage (%d tests); the build "
              "or the runner is broken" % no_cov, file=sys.stderr)
        return 1
    write_json(table, out)
    print("it-coverage-map: %d tests, %d files, %d tests without coverage -> %s"
          % (len(table["tests"]), len(table["files"]), no_cov, out),
          file=sys.stderr)
    return 0


def now_utc():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def cmd_assemble(a, gated=None):
    with open(a.list, encoding="utf-8") as f:
        listing = json.load(f)
    if gated is None:
        gated = platform_gated_files(a.root, a.commit)
    table = assemble(a.tests_dir, listing, a.junit or [], a.root, a.commit,
                     a.generated_at or now_utc(), a.run_url, platform_name(), gated)
    return write_table(table, a.out)


def cmd_part(a):
    with open(a.list, encoding="utf-8") as f:
        listing = json.load(f)
    write_json(make_part(a.tests_dir, listing, a.junit, a.root, a.commit,
                         a.index, a.count, platform_name()), a.out)
    return 0


def cmd_merge(a):
    parts = []
    for path in sorted(glob.glob(os.path.join(a.parts_dir, "*.json"))):
        with open(path, encoding="utf-8") as f:
            parts.append(json.load(f))
    problems, listed, results, exports, platform = merge_parts(parts, a.count, a.commit)
    if problems:
        for problem in problems:
            print("it-coverage-map: merge: " + problem, file=sys.stderr)
        print("it-coverage-map: merge: no table", file=sys.stderr)
        return 1
    table = build_table(listed, results, exports, a.commit,
                        a.generated_at or now_utc(), a.run_url, platform,
                        platform_gated_files(a.root, a.commit))
    return write_table(table, a.out)


def cmd_self_test(a):
    fx = a.fixtures
    with open(os.path.join(fx, "list.json"), encoding="utf-8") as f:
        listing = json.load(f)
    # The fixtures' exports name files under /work/dagq; root is that path
    # whether or not it exists (realpath leaves a missing path as it is).
    got = assemble(os.path.join(fx, "tests"), listing,
                   [os.path.join(fx, "junit.xml")], "/work/dagq",
                   "0123456789abcdef0123456789abcdef01234567",
                   "2026-10-06T18:00:00Z", None, "linux",
                   ["src/infrastructure/launchd.rs"])
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
        cmd_assemble(ns, gated=["src/infrastructure/launchd.rs"])
        with open(out, encoding="utf-8") as f:
            # The OS is this machine's; the fixture says linux.
            written = dict(json.load(f), platform="linux")
            if written != want:
                print("it-coverage-map: self-test: the written table differs",
                      file=sys.stderr)
                return 1
    return self_test_merge(fx, listing, want) or self_test_gated(fx)


def self_test_merge(fx, listing, want):
    """Two parts of the fixtures merge into expected.json's table (less the test
    that did not run), and each way a part can be wrong leaves no table."""
    commit = want["commit"]
    # The fixture's list has a test no part ran; the good case leaves it out.
    good = json.loads(json.dumps(listing))
    del good["rust-suites"]["dagq::it"]["testcases"]["cli_read::did_not_run"]
    expected = json.loads(json.dumps(want))
    del expected["tests"]["dagq::it::cli_read::did_not_run"]
    groups = {1: ["it/claims-the-ready-task", "it/reads-the-status"],
              2: ["broker-serve/starts-and-stops"]}

    def parts_of(the_listing):
        out = []
        for index, dirs in groups.items():
            with tempfile.TemporaryDirectory() as tmp:
                for d in dirs:
                    shutil.copytree(os.path.join(fx, "tests", d), os.path.join(tmp, d))
                out.append(make_part(tmp, the_listing,
                                     [os.path.join(fx, "junit-part-%d.xml" % index)],
                                     "/work/dagq", commit, index, 2, "linux"))
        return out

    parts = parts_of(good)
    problems, listed, results, exports, platform = merge_parts(parts, 2, commit)
    if problems:
        print("it-coverage-map: self-test: merge: %r" % problems, file=sys.stderr)
        return 1
    got = build_table(listed, results, exports, commit, want["generated_at"], None,
                      platform, want["platform_gated_files"])
    if got != expected:
        print("it-coverage-map: self-test: the merged table differs from "
              "expected.json", file=sys.stderr)
        print(json.dumps(got, indent=1, sort_keys=True), file=sys.stderr)
        return 1

    def changed(index, **fields):
        out = json.loads(json.dumps(parts))
        out[index].update(fields)
        return out

    twice = json.loads(json.dumps(parts))
    twice[1]["results"].append(twice[0]["results"][0])
    unlisted = json.loads(json.dumps(parts))
    unlisted[1]["results"].append(["dagq::it", "extra", 1.0, "passed"])
    bad = [
        ("a part missing", parts[:1], "missing parts: [2]"),
        ("no part", [], "missing parts: [1, 2]"),
        ("another commit", changed(1, commit="f" * 40), "commit"),
        ("another count", changed(1, partition={"index": 2, "count": 3}), "of 3 partitions"),
        ("the same part twice", [parts[0], parts[0], parts[1]], "more than once"),
        ("another OS", changed(1, platform="macos"), "different OSes"),
        ("another list", changed(1, listed=parts[1]["listed"][1:]), "another list of tests"),
        ("a test in two parts", twice, "in another part too"),
        ("a listed test that did not run", parts_of(listing), "did not run"),
        ("a test not listed", unlisted, "not listed"),
        ("a part without coverage", changed(1, covered=[]), "no test has coverage"),
    ]
    for name, case, needle in bad:
        problems = merge_parts(case, 2, commit)[0]
        if not any(needle in p for p in problems):
            print("it-coverage-map: self-test: merge with %s: %r" % (name, problems),
                  file=sys.stderr)
            return 1

    # cmd_merge writes no table on a problem. (The table it does write needs a
    # repository for the commit's files: the fake cargo case of the sh self-test.)
    with tempfile.TemporaryDirectory() as tmp:
        write_json(parts[0], os.path.join(tmp, "1.json"))
        out = os.path.join(tmp, "map.json")
        ns = argparse.Namespace(parts_dir=tmp, count=2, root="/work/dagq", commit=commit,
                                generated_at=None, run_url=None, out=out)
        if cmd_merge(ns) != 1 or os.path.exists(out):
            print("it-coverage-map: self-test: a missing part wrote a table",
                  file=sys.stderr)
            return 1
    statuses = {k: t["status"] for k, t in got["tests"].items()}
    if statuses != {"dagq-broker::serve::serve::starts_and_stops": "flaky",
                    "dagq::it::cli_read::reads_the_status": "failed",
                    "dagq::it::runtime_claim::claims_the_ready_task": "passed"}:
        print("it-coverage-map: self-test: merged statuses: %r" % statuses,
              file=sys.stderr)
        return 1
    return 0


def self_test_gated(fx):
    """The files with an OS condition in a commit's tree, in a throwaway
    repository of the fixture sources, and not those of the work tree."""
    source = os.path.join(fx, "platform-gated")
    want = ["crates/tool/src/lib.rs", "src/after_escaped_char.rs",
            "src/any_over_lines.rs", "src/attribute.rs", "src/macro_call.rs",
            "src/vendor_apple.rs"]
    with tempfile.TemporaryDirectory() as tmp:
        repo = os.path.join(tmp, "repo")
        shutil.copytree(source, repo)
        env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        for args in (["init", "-q"], ["add", "."],
                     ["-c", "user.name=self-test",
                      "-c", "user.email=self-test@example.invalid",
                      "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "fixture"]):
            subprocess.run(["git", "-C", repo] + args, check=True, env=env,
                           capture_output=True)
        commit = subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], check=True, env=env,
                                capture_output=True, text=True).stdout.strip()
        with open(os.path.join(repo, "src", "not_committed.rs"), "w", encoding="utf-8") as f:
            f.write('#[cfg(target_os = "macos")]\nfn f() {}\n')
        got = platform_gated_files(repo, commit)
    if got != want:
        print("it-coverage-map: self-test: platform_gated_files: %r, want %r" % (got, want),
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
    r = sub.add_parser("part")
    r.add_argument("--tests-dir", required=True)
    r.add_argument("--list", required=True)
    r.add_argument("--junit", action="append", required=True)
    r.add_argument("--root", required=True)
    r.add_argument("--commit", required=True)
    r.add_argument("--index", type=int, required=True)
    r.add_argument("--count", type=int, required=True)
    r.add_argument("--out", required=True)
    m = sub.add_parser("merge")
    m.add_argument("--parts-dir", required=True)
    m.add_argument("--count", type=int, required=True)
    m.add_argument("--root", required=True)
    m.add_argument("--commit", required=True)
    m.add_argument("--out", required=True)
    m.add_argument("--generated-at")
    m.add_argument("--run-url")
    t = sub.add_parser("self-test")
    t.add_argument("fixtures")
    a = p.parse_args(argv)
    return {"export": cmd_export, "assemble": cmd_assemble, "part": cmd_part,
            "merge": cmd_merge, "self-test": cmd_self_test}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
