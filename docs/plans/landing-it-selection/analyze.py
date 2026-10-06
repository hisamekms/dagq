#!/usr/bin/env python3
"""Applies the IT coverage map to the past landings' diffs
(docs/plans/landing-it-selection.md).

Usage: python3 analyze.py DATA MAP [--repo REPO] [--out OUT]
         [--threshold-wall SECS] [--threshold-ratio R]
  DATA  what collect.sh wrote (landings.tsv, ci-runs.json,
        ci-failed-tests.tsv; maps/ for the staleness comparison)
  MAP   the it-coverage-map.json the selection uses (the latest one)
  OUT   where the CSVs and summary.txt go (default: this directory)

It reads the queue's records, git and CI's history only, so it gives the same
result on every run: there is no host load or order to repeat it for.
"""
import argparse
import csv
import datetime as dt
import fnmatch
import json
import math
import os
import re
import subprocess
from collections import Counter, defaultdict

THREADS = 6  # dagq.toml [run.env] NEXTEST_TEST_THREADS

# A diff touching one of these runs every IT: build inputs the coverage map
# cannot see (they change every test binary) and the shared fixtures of
# tests/common (89% of the tests go through tests/common/mod.rs and
# template.rs). tests/it/runtime_support and the crates' tests/common are
# picked by the map like src (each of their files has its own tests), and
# tests/it/main.rs holds only the mod lines (no test executes a line of it; the
# module it adds is a changed test file).
COMMON = [
    "Cargo.lock",
    "Cargo.toml",
    "crates/*/Cargo.toml",
    "build.rs",
    "crates/*/build.rs",
    "rust-toolchain.toml",
    ".config/nextest.toml",
    "migrations/*",
    "src/migration_numbers.rs",
    "tests/common/*",
]

# Files outside src/ that tests read from the repository: the tests they pick.
REPO_FILES = [
    ("plugins/*", ["dagq::plugin::", "dagq::it::installed_plugin::"]),
    ("scripts/check-migration-numbers.sh", ["dagq::it::queue_schema::"]),
    ("dagq.toml", ["dagq::it::review_subagents::this_repository_names_only_agents_it_defines"]),
    (".dagq/agents/*", ["dagq::it::review_subagents::this_repository_names_only_agents_it_defines"]),
]

# The CI run with this many tests newly red at once is counted apart: the
# whole suite turned red for one cause that no landing's selection is about
# (the fixture template cache restored empty, ci.yml's comment).
MASS = 50


def match(path, pattern):
    # fnmatch's * spans "/", which is what these patterns want.
    return fnmatch.fnmatchcase(path, pattern)


def median(values):
    v = sorted(values)
    n = len(v)
    if n == 0:
        return None
    if n % 2:
        return v[(n + 1) // 2 - 1]
    return (v[n // 2 - 1] + v[n // 2]) / 2


def p90(values):
    v = sorted(values)
    if not v:
        return None
    return v[math.ceil(0.9 * len(v)) - 1]


def git(repo, *args, check=True):
    r = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr}")
    return r


class Map:
    def __init__(self, path):
        d = json.load(open(path))
        self.commit = d["commit"]
        self.generated_at = d["generated_at"]
        self.run_url = d["run_url"]
        self.tests = d["tests"]
        self.files = d["files"]
        self.dur = {k: t["duration_secs"] for k, t in self.tests.items()}
        self.all = set(self.tests)
        self.tree = set()

    def load_tree(self, repo):
        self.tree = set(git(repo, "ls-tree", "-r", "--name-only", self.commit).stdout.split())

    def prefixed(self, prefix):
        return {k for k in self.tests if k.startswith(prefix)}

    def time(self, tests):
        serial = sum(self.dur[t] for t in tests)
        longest = max((self.dur[t] for t in tests), default=0.0)
        return serial, max(serial / THREADS, longest)


def tests_of_test_file(m, path):
    """The tests a changed test file defines."""
    mo = re.fullmatch(r"tests/it/([a-z0-9_]+)\.rs", path)
    if mo:
        return m.prefixed(f"dagq::it::{mo.group(1)}::")
    if path == "tests/plugin.rs":
        return m.prefixed("dagq::plugin::")
    mo = re.fullmatch(r"crates/([a-z0-9_-]+)/tests/([a-z0-9_]+)\.rs", path)
    if mo:
        return m.prefixed(f"{mo.group(1)}::{mo.group(2)}::")
    return set()


def select(m, files, threshold_wall, threshold_ratio):
    """(full, reason, tests, narrowed tests before the threshold)."""
    common = [f for f in files if any(match(f, p) for p in COMMON)]
    if common:
        return True, "common", m.all, None
    tests = set()
    unknown = []
    for f in files:
        if f in m.files:
            tests |= set(m.files[f])
        tests |= tests_of_test_file(m, f)
        for pattern, prefixes in REPO_FILES:
            if match(f, pattern):
                for p in prefixes:
                    tests |= m.prefixed(p)
        if f.endswith(".rs") and (f.startswith("src/") or re.match(r"crates/[^/]+/src/", f)) and f not in m.files:
            if re.search(r"(^|/)tests(/|\.rs$)|_tests\.rs$", f):
                continue  # a unit test module: unit tests all run anyway
            if f in m.tree:
                continue  # in the table's tree, but no IT executes a line of it
            unknown.append(f)  # not in the table's tree: the table cannot tell
    if unknown:
        return True, "other", m.all, tests
    serial, wall = m.time(tests)
    if (threshold_wall is not None and wall > threshold_wall) or (
        threshold_ratio is not None and len(tests) > threshold_ratio * len(m.all)
    ):
        return True, "threshold", m.all, tests
    return False, "", tests, tests


def test_fns(repo, commit, cache):
    """The integration test functions at a commit: {path::fn}."""
    if commit in cache:
        return cache[commit]
    r = git(repo, "grep", "-n", "-E", r"^\s*(#\[(tokio::)?test|(pub )?(async )?fn [a-zA-Z0-9_]+)",
            commit, "--", "tests/it", "tests/plugin.rs", ":(glob)crates/*/tests/**", check=False)
    out = set()
    pending = {}
    for line in r.stdout.splitlines():
        _, path, _, text = line.split(":", 3)
        if re.match(r"\s*#\[(tokio::)?test", text):
            pending[path] = True
            continue
        mo = re.match(r"\s*(?:pub )?(?:async )?fn ([a-zA-Z0-9_]+)", text)
        if mo and pending.pop(path, False):
            out.add(f"{path}::{mo.group(1)}")
    cache[commit] = out
    return out


def src_count(repo, commit, cache):
    if commit not in cache:
        names = git(repo, "ls-tree", "-r", "--name-only", commit, "--", "src", "crates").stdout.split()
        cache[commit] = sum(1 for n in names if n.endswith(".rs") and (n.startswith("src/") or re.match(r"crates/[^/]+/src/", n)))
    return cache[commit]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("map")
    ap.add_argument("--repo", default=".")
    ap.add_argument("--out", default=os.path.dirname(os.path.abspath(__file__)))
    ap.add_argument("--threshold-wall", type=float, default=None)
    ap.add_argument("--threshold-ratio", type=float, default=None)
    a = ap.parse_args()
    repo = a.repo
    m = Map(a.map)
    m.load_tree(repo)

    # Landings and their diffs.
    landings, missing = [], []
    for line in open(os.path.join(a.data, "landings.tsv")):
        eid, at, task, run, before, commit, skipped = line.rstrip("\n").split("\t")
        row = dict(event=int(eid), at=at, task=int(task), run=run, before=before, commit=commit)
        if any(git(repo, "cat-file", "-e", f"{c}^{{commit}}", check=False).returncode for c in (before, commit)):
            missing.append(row)
            continue
        row["files"] = git(repo, "diff", "--name-only", before, commit).stdout.split()
        full, reason, tests, narrowed = select(m, row["files"], a.threshold_wall, a.threshold_ratio)
        row.update(full=full, reason=reason, tests=tests, narrowed=narrowed)
        row["serial"], row["wall"] = m.time(tests)
        landings.append(row)
    by_commit = {r["commit"]: r for r in landings}

    # CI: each job's runs with test results, in order, and the tests each
    # turned red (a test red in a job whose previous result had it green).
    runs = [r for r in json.load(open(os.path.join(a.data, "ci-runs.json"))) if r["conclusion"] != "cancelled"]
    runs.sort(key=lambda r: r["createdAt"])
    order = {r["databaseId"]: i for i, r in enumerate(runs)}
    failed = defaultdict(set)  # (run, job) -> tests
    for line in open(os.path.join(a.data, "ci-failed-tests.tsv")):
        rid, job, test, kind = line.rstrip("\n").split("\t")
        if kind == "fail":
            failed[(int(rid), job)].add(test)
    results = defaultdict(list)  # job -> runs with a test result
    no_results = []
    for line in open(os.path.join(a.data, "ci-jobs.tsv")):
        rid, name, step = line.rstrip("\n").split("\t")
        rid, job = int(rid), "linux" if "linux" in name.lower() else "macos"
        if step == "success" or (step == "failure" and failed.get((rid, job))):
            results[job].append(rid)
        else:
            no_results.append((rid, job, step))
    it_binaries = {t["binary_id"] for t in m.tests.values()}
    by_id = {r["databaseId"]: r for r in runs}
    new_by_run = defaultdict(dict)  # run -> test -> (prev run, still red next run)
    for job, rids in results.items():
        rids.sort(key=order.get)
        for i in range(1, len(rids)):
            prev, cur = rids[i - 1], rids[i]
            nxt = rids[i + 1] if i + 1 < len(rids) else None
            for t in failed.get((cur, job), set()) - failed.get((prev, job), set()):
                p0 = new_by_run[cur].get(t)
                # Red in both jobs: the range is the shorter one (the later previous run).
                if p0 is None or order[prev] > order[p0[0]]:
                    new_by_run[cur][t] = (prev, nxt is not None and t in failed.get((nxt, job), set()))
    events = []
    for cur in sorted(new_by_run, key=order.get):
        r = by_id[cur]
        new = new_by_run[cur]
        for t in sorted(new):
            prev, persistent = new[t]
            rng = git(repo, "rev-list", "--first-parent", f"{by_id[prev]['headSha']}..{r['headSha']}").stdout.split()
            in_range = [by_commit[c] for c in rng if c in by_commit]
            direct = [c for c in rng if c not in by_commit]
            if t not in m.tests:
                if any(t.startswith(b + "::") for b in it_binaries):
                    cls = "it not in the table"
                elif t.startswith("dagq::e2e::"):
                    cls = "e2e"
                else:
                    cls = "unit (always run)"
                selected = None
            else:
                cls = "it"
                selected = [x["task"] for x in in_range if t in x["tests"]]
            events.append(dict(
                test=t, prev_run=prev, red_run=cur, red_at=r["createdAt"],
                range_tasks=[x["task"] for x in in_range], direct=direct, new_in_run=len(new), cls=cls,
                selected=selected, persistent=persistent,
            ))
    for e in events:
        e["mass"] = e["new_in_run"] >= MASS
        e["multi"] = len(e["range_tasks"]) > 1
        # A commit that reached main without landing through dagq (a person's
        # direct commit) had no selection: a test red in a range holding one
        # is counted apart, not as a miss.
        e["missed"] = e["cls"] == "it" and not e["selected"] and not e["direct"]
        e["direct_unselected"] = e["cls"] == "it" and not e["selected"] and bool(e["direct"])
    missed_by_task = defaultdict(list)
    for e in events:
        if e["missed"] and len(e["range_tasks"]) == 1:
            missed_by_task[e["range_tasks"][0]].append(e["test"])

    # Staleness material.
    fns_cache, src_cache, stale = {}, {}, []
    for r in landings:
        at = dt.datetime.strptime(r["at"][:19], "%Y-%m-%dT%H:%M:%S")
        now = test_fns(repo, r["before"], fns_cache)
        total = src_count(repo, r["before"], src_cache)
        for n in (1, 2, 3, 7):
            base = git(repo, "rev-list", "-1", "--first-parent", f"--before={(at - dt.timedelta(days=n)).isoformat()}Z", r["before"]).stdout.strip()
            then = test_fns(repo, base, fns_cache)
            # By path and name (a moved test is new to the table, whose key
            # holds its module), and by name only (a test that is new).
            added = len(now - then)
            then_names = {k.rsplit("::", 1)[1] for k in then}
            added_names = len({k.rsplit("::", 1)[1] for k in now} - then_names)
            # The files present at the landing that changed (a deleted file is
            # not one a selection can pick).
            changed = sum(1 for f in git(repo, "diff", "--name-only", "--diff-filter=d", base, r["before"], "--", "src", "crates").stdout.split()
                          if f.endswith(".rs") and (f.startswith("src/") or re.match(r"crates/[^/]+/src/", f)))
            stale.append(dict(task=r["task"], at=r["at"], commit=r["commit"], n=n, base=base, added=added, added_names=added_names,
                              changed=changed, total=total, ratio=changed / total if total else 0.0))

    # Other maps still kept: the same diffs with an older table.
    others = []
    maps_dir = os.path.join(a.data, "maps")
    if os.path.isdir(maps_dir):
        for name in sorted(os.listdir(maps_dir)):
            o = Map(os.path.join(maps_dir, name))
            o.load_tree(repo)
            if o.commit == m.commit:
                continue
            diff_counts = []
            for r in landings:
                _, _, t2, _ = select(o, r["files"], a.threshold_wall, a.threshold_ratio)
                diff_counts.append(len(r["tests"] ^ t2))
            others.append((o, diff_counts))

    os.makedirs(a.out, exist_ok=True)
    with open(os.path.join(a.out, "landings.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["event_id", "landed_at", "task", "commit", "diff_files", "full", "reason", "selected_tests",
                    "serial_secs", "wall_secs", "narrowed_tests_before_threshold", "narrowed_wall_secs_before_threshold",
                    "missed_tests"])
        for r in landings:
            nw = "" if r["narrowed"] is None else round(m.time(r["narrowed"])[1], 1)
            nn = "" if r["narrowed"] is None else len(r["narrowed"])
            w.writerow([r["event"], r["at"], r["task"], r["commit"], len(r["files"]), int(r["full"]), r["reason"],
                        len(r["tests"]), round(r["serial"], 1), round(r["wall"], 1), nn, nw,
                        ";".join(missed_by_task.get(r["task"], []))])
    with open(os.path.join(a.out, "ci-new-failures.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["test", "prev_run", "red_run", "red_at", "new_in_run", "mass", "range_tasks", "class",
                    "selected_by", "missed", "multi_landing_range", "direct_commits_in_range", "still_red_next_run"])
        for e in events:
            w.writerow([e["test"], e["prev_run"], e["red_run"], e["red_at"], e["new_in_run"], int(e["mass"]),
                        ";".join(map(str, e["range_tasks"])), e["cls"],
                        "" if e["selected"] is None else ";".join(map(str, e["selected"])),
                        int(e["missed"]), int(e["multi"]), ";".join(c[:12] for c in e["direct"]), int(e["persistent"])])
    with open(os.path.join(a.out, "staleness.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["task", "commit", "days", "base_commit", "added_or_moved_tests", "added_test_names", "changed_src_files", "src_files", "changed_ratio"])
        for s in stale:
            w.writerow([s["task"], s["commit"], s["n"], s["base"], s["added"], s["added_names"], s["changed"], s["total"], round(s["ratio"], 4)])

    # Summary.
    def fmt(x):
        return "-" if x is None else f"{x:.1f}"
    L = landings
    narrowed = [r for r in L if not r["full"]]
    reasons = Counter(r["reason"] for r in L if r["full"])
    full_serial, full_wall = m.time(m.all)
    out = []
    out.append(f"map: commit {m.commit}, generated_at {m.generated_at}, {len(m.all)} tests, run {m.run_url}")
    out.append(f"all IT: serial {full_serial:.1f} s, wall {full_wall:.1f} s")
    out.append(f"threshold: wall {a.threshold_wall} s, ratio {a.threshold_ratio}")
    out.append(f"landings: {len(L) + len(missing)} read, {len(missing)} not in git, {len(L)} measured")
    out.append(f"landings touching no test-relevant file (0 tests selected): {sum(1 for r in L if not r['tests'])}")
    for label, rows in (("all", L), ("narrowed only", narrowed)):
        out.append(f"{label} (n={len(rows)}): serial median {fmt(median([r['serial'] for r in rows]))} p90 {fmt(p90([r['serial'] for r in rows]))}; "
                   f"wall median {fmt(median([r['wall'] for r in rows]))} p90 {fmt(p90([r['wall'] for r in rows]))}; "
                   f"tests median {fmt(median([len(r['tests']) for r in rows]))} p90 {fmt(p90([len(r['tests']) for r in rows]))}")
    out.append(f"full: {len(L) - len(narrowed)} / {len(L)} = {(len(L) - len(narrowed)) / len(L):.3f}; reasons {dict(reasons)}")
    nb = [m.time(r["narrowed"])[1] for r in L if r["narrowed"] is not None]
    out.append(f"narrowed wall before the threshold (n={len(nb)}): median {fmt(median(nb))} p90 {fmt(p90(nb))}")
    out.append(f"CI runs: {len(runs)} not cancelled; jobs with test results: "
               + ", ".join(f"{j} {len(v)}" for j, v in sorted(results.items()))
               + f"; jobs without: {len(no_results)} ({', '.join(f'{r}/{j}/{st}' for r, j, st in no_results)})")
    ev = events
    cls = Counter(e["cls"] for e in ev)
    out.append(f"tests newly red: {len(ev)}; by class {dict(cls)}")
    for label, sel in (("all", ev), ("mass runs", [e for e in ev if e["mass"]]), ("other runs", [e for e in ev if not e["mass"]])):
        it = [e for e in sel if e["cls"] == "it"]
        multi = [e for e in it if e["multi"]]
        out.append(f"  {label}: it {len(it)}; missed {sum(e['missed'] for e in it)} "
                   f"(single-landing range {sum(e['missed'] and not e['multi'] for e in it)}, multi-landing {sum(e['missed'] and e['multi'] for e in it)}); "
                   f"in a multi-landing range {len(multi)}, of them selected by some landing {sum(1 for e in multi if e['selected'])} "
                   f"(by only part of the range {sum(1 for e in multi if e['selected'] and len(e['selected']) < len(e['range_tasks']))}); "
                   f"missed and still red next run {sum(e['missed'] and e['persistent'] for e in it)}; "
                   f"unselected in a range with a direct commit {sum(e['direct_unselected'] for e in it)}")
    # The staleness material over all landings, and over those whose 3-day
    # window starts after the tests moved into tests/it (2026-09-26 to 28).
    for label, since in (("all", ""), ("landed from 2026-10-03", "2026-10-03")):
        for n in (1, 2, 3, 7):
            s = [x for x in stale if x["n"] == n and x["at"] >= since]
            out.append(f"stale {label} N={n} (n={len(s)}): "
                       f"added or moved tests median {fmt(median([x['added'] for x in s]))} p90 {fmt(p90([x['added'] for x in s]))}; "
                       f"added test names median {fmt(median([x['added_names'] for x in s]))} p90 {fmt(p90([x['added_names'] for x in s]))}; "
                       f"changed src median {fmt(median([x['changed'] for x in s]))} p90 {fmt(p90([x['changed'] for x in s]))}; "
                       f"ratio median {median([x['ratio'] for x in s]):.3f} p90 {p90([x['ratio'] for x in s]):.3f}")
    if others:
        for o, counts in others:
            out.append(f"older map {o.commit} {o.generated_at}: selection differs in {sum(1 for c in counts if c)} landings, "
                       f"median {fmt(median(counts))} p90 {fmt(p90(counts))} tests")
    else:
        out.append("older maps: none kept besides the one used (fewer than 2 artifacts)")
    text = "\n".join(out) + "\n"
    open(os.path.join(a.out, "summary.txt"), "w").write(text)
    print(text)


if __name__ == "__main__":
    main()
