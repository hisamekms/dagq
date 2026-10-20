#!/usr/bin/env python3
"""Applies coverage maps of different dates to the same landings' diffs and
counts how the narrowed IT differ and what the older map would have let
through (docs/plans/landing-it-selection.md, 「表の古さの追加の測定」).

Usage: python3 compare_maps.py DATA T_END [--repo REPO] [--out OUT]
  DATA   what collect.sh wrote (landings.tsv, ci-runs.json, ci-failed-tests.tsv,
         ci-jobs.tsv and every kept map in maps/)
  T_END  the end of the period; the landings read are those before it from
         the oldest map's generated_at
  OUT    where the CSVs and the summary go (default: this directory):
         map-staleness.csv (one row per landing and k),
         map-staleness-single-failures.csv and
         map-staleness-multi-failures.csv (CI's new failures by their range)
         and map-staleness-summary.txt

For each landing, B is the latest map generated before it and A_k the map k
(1, 2, 3) maps before B. Both select with scripts/landing-it.py's rules but
the table's age (the counterfactual without MAX_TABLE_AGE): the common files,
the broad entries, the threshold, the diff's tests and the files tests read.
U, the IT at the landing, is B's tests (nextest's list at the landing is not
kept); a full run selects U, and A's narrowed selection is
(A's tests selected ∩ U) ∪ (U - A's tests), as landing-it.py always runs the
IT not in its table.

It reads records, git and the maps only, so it gives the same result on every
run: there is no host load or order to repeat it for.
"""
import argparse
import csv
import datetime as dt
import importlib.util
import json
import os
import re
import sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from analyze import MASS, ci_new_failures, git, median, p90  # noqa: E402

KS = (1, 2, 3)
# The age buckets in hours: [lo, hi).
BUCKETS = [("<24h", 0, 24), ("24-48h", 24, 48), ("48-72h", 48, 72), (">=72h", 72, None)]


def load_landing_it(repo):
    spec = importlib.util.spec_from_file_location("landing_it", os.path.join(repo, "scripts", "landing-it.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def parse(text):
    return dt.datetime.strptime(text[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=dt.timezone.utc)


def bucket(hours):
    for name, lo, hi in BUCKETS:
        if hours >= lo and (hi is None or hours < hi):
            return name
    raise ValueError(hours)


def filter_tests(expr, universe):
    """The tests of U one of landing-it.py's filtersets for a test file or a
    file tests read names: binary_id(=B), optionally & test(/^P::/) or
    & test(=N)."""
    b = re.search(r"binary_id\(=([^)]+)\)", expr).group(1)
    prefix = re.search(r"test\(/\^(.*)/\)", expr)
    exact = re.search(r"test\(=(.*)\)\)$", expr)
    out = set()
    for key, (binary, name) in universe.items():
        if binary != b:
            continue
        if prefix and not name.startswith(prefix.group(1)):
            continue
        if exact and name != exact.group(1):
            continue
        out.add(key)
    return out


def select(lit, table, diff, universe, threads):
    """(full by a reason besides the age, the selected tests of U)."""
    now = lit.parse_time(table["generated_at"])  # age 0: MAX_TABLE_AGE is left out
    U = set(universe)
    if lit.full_reasons(diff, [], table, None, now):
        return True, U
    files, tt = table["files"], table["tests"]
    limit = lit.BROAD_SHARE * len(tt)
    from_table = set()
    filters = []
    for path in diff:
        if len(files.get(path, [])) <= limit:
            from_table.update(k for k in files.get(path, []) if k in tt)
        f = lit.test_file_filter(path)
        if f is not None:
            filters.append(f)
        for pattern, fs in lit.READ_BY_TESTS:
            if lit.glob_match(path, pattern):
                filters.extend(fs)
    all_wall = lit.wall([t.get("duration_secs") or 0.0 for t in tt.values()], threads)
    estimate = lit.wall([tt[k].get("duration_secs") or 0.0 for k in from_table], threads)
    if estimate > lit.FULL_SHARE * all_wall:
        return True, U
    sel = (from_table & U) | (U - set(tt))
    for f in filters:
        sel |= filter_tests(f, universe)
    return False, sel


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("t_end")
    ap.add_argument("--repo", default=".")
    ap.add_argument("--out", default=HERE)
    ap.add_argument("--threads", type=int, default=6)  # dagq.toml [run.env] NEXTEST_TEST_THREADS
    a = ap.parse_args()
    repo, t_end = a.repo, parse(a.t_end)
    lit = load_landing_it(repo)

    maps = []
    mdir = os.path.join(a.data, "maps")
    for name in os.listdir(mdir):
        d = json.load(open(os.path.join(mdir, name)))
        d["run_id"] = name[:-5]
        d["at"] = parse(d["generated_at"])
        maps.append(d)
    maps.sort(key=lambda d: d["at"])
    since = maps[0]["at"]

    landings, missing = [], []
    for line in open(os.path.join(a.data, "landings.tsv")):
        eid, at, task, run, before, commit, _ = line.rstrip("\n").split("\t")
        when = parse(at)
        if not (since <= when < t_end):
            continue
        row = dict(event=int(eid), at=at, when=when, task=int(task), before=before, commit=commit)
        if any(git(repo, "cat-file", "-e", f"{c}^{{commit}}", check=False).returncode for c in (before, commit)):
            missing.append(row)
            continue
        row["files"] = git(repo, "diff", "--name-only", before, commit).stdout.split()
        landings.append(row)

    # The comparisons: (landing, k) -> dict.
    cache = {}

    def sel(mi, row, universe):
        key = (mi, row["event"], id(universe))
        if key not in cache:
            cache[key] = select(lit, maps[mi], row["files"], universe, a.threads)
        return cache[key]

    universes = {}
    comps = []
    for row in landings:
        bi = max(i for i, m in enumerate(maps) if m["at"] <= row["when"])
        if bi not in universes:
            universes[bi] = {k: (t["binary_id"], t["name"]) for k, t in maps[bi]["tests"].items()}
        U = universes[bi]
        row["b"] = bi
        b_full, b_sel = sel(bi, row, U)
        row["b_sel"] = b_sel
        row["cmp"] = {}
        for k in KS:
            ai = bi - k
            if ai < 0:
                continue
            a_full, a_sel = sel(ai, row, U)
            age = (row["when"] - maps[ai]["at"]).total_seconds() / 3600
            c = dict(row=row, k=k, b=bi, a=ai, age=age, bucket=bucket(age), b_full=b_full, a_full=a_full,
                     only_b=b_sel - a_sel, only_a=a_sel - b_sel,
                     full_differs=b_full != a_full, full_by_limit=age > lit.MAX_TABLE_AGE.total_seconds() / 3600)
            row["cmp"][k] = c
            comps.append(c)

    # CI's new failures, as in analyze.py; a range reaching before the
    # period (its previous run before the oldest map) is left out.
    by_commit = {r["commit"]: r for r in landings}
    runs, results, no_results, raw = ci_new_failures(a.data, repo, by_commit)
    before_period = [e for e in raw if parse(e["prev_at"]) < since]
    events = [e for e in raw if parse(e["prev_at"]) >= since]
    single = [e for e in events if len(e["in_range"]) == 1 and not e["direct"]]
    multi = [e for e in events if not (len(e["in_range"]) == 1 and not e["direct"])]
    missed = defaultdict(list)  # (k, bucket) -> events
    for e in single:
        row = e["in_range"][0]
        for k, c in row["cmp"].items():
            if e["test"] in c["only_b"]:
                missed[(k, c["bucket"])].append(e)

    os.makedirs(a.out, exist_ok=True)
    rid = lambda i: maps[i]["run_id"]  # noqa: E731
    with open(os.path.join(a.out, "map-staleness.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["task", "commit", "landed_at", "k", "map_b", "map_a", "age_hours", "only_b_tests", "only_a_tests",
                    "full_differs_besides_age", "full_by_current_limit", "b_full", "a_full", "b_selected", "a_selected"])
        for c in comps:
            r = c["row"]
            w.writerow([r["task"], r["commit"], r["at"], c["k"], rid(c["b"]), rid(c["a"]), round(c["age"], 1),
                        len(c["only_b"]), len(c["only_a"]), int(c["full_differs"]), int(c["full_by_limit"]),
                        int(c["b_full"]), int(c["a_full"]), len(r["b_sel"]), len(r["b_sel"]) - len(c["only_b"]) + len(c["only_a"])])
    with open(os.path.join(a.out, "map-staleness-single-failures.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["test", "red_run", "red_at", "mass", "task", "commit", "selected_by_b"]
                   + [x for k in KS for x in (f"bucket_k{k}", f"only_b_k{k}")])
        for e in single:
            r = e["in_range"][0]
            cols = []
            for k in KS:
                c = r["cmp"].get(k)
                cols += ["", ""] if c is None else [c["bucket"], int(e["test"] in c["only_b"])]
            w.writerow([e["test"], e["red_run"], e["red_at"], int(e["new_in_run"] >= MASS), r["task"], r["commit"],
                        int(e["test"] in r["b_sel"])] + cols)
    with open(os.path.join(a.out, "map-staleness-multi-failures.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["test", "red_run", "red_at", "mass", "range_tasks", "direct_commits_in_range",
                    "selected_by_b"] + [f"only_b_k{k}" for k in KS])
        for e in multi:
            rows = e["in_range"]
            w.writerow([e["test"], e["red_run"], e["red_at"], int(e["new_in_run"] >= MASS),
                        ";".join(str(r["task"]) for r in rows), ";".join(c[:12] for c in e["direct"]),
                        ";".join(str(r["task"]) for r in rows if e["test"] in r["b_sel"])]
                       + [";".join(str(r["task"]) for r in rows if k in r["cmp"] and e["test"] in r["cmp"][k]["only_b"]) for k in KS])

    out = []
    out.append(f"period: {maps[0]['generated_at']} to {a.t_end}; landings {len(landings) + len(missing)} read, "
               f"{len(missing)} not in git, {len(landings)} measured")
    for i, m in enumerate(maps):
        used = sum(1 for r in landings if r["b"] == i)
        out.append(f"map {m['run_id']} commit {m['commit'][:12]} generated_at {m['generated_at']} tests {len(m['tests'])} "
                   f"files {len(m['files'])}; B of {used} landings")
    for k in KS:
        cs = [c for c in comps if c["k"] == k]
        out.append(f"k={k}: {len(cs)} comparisons; full differs besides the age {sum(c['full_differs'] for c in cs)}; "
                   f"full by the current limit {sum(c['full_by_limit'] for c in cs)}")
        for name, _, _ in BUCKETS:
            bc = [c for c in cs if c["bucket"] == name]
            ms = missed[(k, name)]
            if not bc:
                out.append(f"  {name}: n=0 (no sample)")
                continue
            diff = sum(1 for c in bc if c["only_b"] or c["only_a"])
            ob = [len(c["only_b"]) for c in bc]
            oa = [len(c["only_a"]) for c in bc]
            out.append(f"  {name}: n={len(bc)}; differ {diff} ({diff / len(bc):.3f}); "
                       f"only_b median {median(ob)} p90 {p90(ob)} max {max(ob)}; only_b>0 {sum(1 for x in ob if x)} "
                       f"(both narrowed {sum(1 for c in bc if c['only_b'] and not c['b_full'] and not c['a_full'])}); "
                       f"only_a median {median(oa)} p90 {p90(oa)}; "
                       f"full differs {sum(c['full_differs'] for c in bc)}; full by limit {sum(c['full_by_limit'] for c in bc)}; "
                       f"single-landing failures B selects and A not {len(ms)} (mass {sum(1 for e in ms if e['new_in_run'] >= MASS)})")
    out.append(f"CI runs {len(runs)} not cancelled; jobs with results "
               + ", ".join(f"{j} {len(v)}" for j, v in sorted(results.items())) + f"; without {len(no_results)}")
    out.append(f"tests newly red: {len(raw)}; range reaching before the period {len(before_period)}; "
               f"single-landing range {len(single)} (in B's selection {sum(1 for e in single if e['test'] in e['in_range'][0]['b_sel'])}, "
               f"mass {sum(1 for e in single if e['new_in_run'] >= MASS)}); "
               f"multi-landing or with a direct commit {len(multi)} "
               f"(with B-only in some landing: " + ", ".join(
                   f"k={k} {sum(1 for e in multi if any(k in r['cmp'] and e['test'] in r['cmp'][k]['only_b'] for r in e['in_range']))}" for k in KS) + ")")
    text = "\n".join(out) + "\n"
    open(os.path.join(a.out, "map-staleness-summary.txt"), "w").write(text)
    print(text)


if __name__ == "__main__":
    main()
