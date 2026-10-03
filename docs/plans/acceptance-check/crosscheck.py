#!/usr/bin/env python3
"""Cross-check the computed run list against `dagq stats` (task 1422).

Reads only OUT/runs.csv (written by compute.py) and
SNAPSHOT/reference/stats.json.gz. Never reads the queue or normalized/.
Writes OUT/crosscheck.csv: one row per run and field where they differ.
The computed tables do not read it back.

  python3 crosscheck.py SNAPSHOT [--out DIR]
"""

import argparse
import csv
import gzip
import json
import os

FIELDS = (
    ("work", lambda s: s.get("work")),
    ("wait_to_land", lambda s: s.get("wait_to_land")),
    ("land_ask", lambda s: (s.get("land_phases") or {}).get("ask")),
    ("route", lambda s: s.get("route")),
    ("actual_provider", lambda s: s.get("actual_provider")),
    ("areas", lambda s: ";".join(s.get("areas") or [])),
)
NOT_A_VALUE = ("未取得", "未着地", "未完")


def shown(value):
    return "null" if value is None else str(value)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("snapshot")
    p.add_argument("--out", default=None)
    a = p.parse_args()
    out = a.out or os.path.join(os.path.dirname(os.path.abspath(__file__)), "out",
                                os.path.basename(os.path.normpath(a.snapshot)))
    with gzip.open(os.path.join(a.snapshot, "reference", "stats.json.gz"), "rt", encoding="utf-8") as f:
        stats = {r["run_id"]: r for r in json.load(f)["runs"]}
    runs = {}
    with open(os.path.join(out, "runs.csv"), encoding="utf-8") as f:
        for row in csv.DictReader(f):
            runs.setdefault(row["run_id"], row)
    rows = []
    for run_id in sorted(runs):
        mine = runs[run_id]
        theirs = stats.get(run_id)
        if theirs is None:
            rows.append({"run_id": run_id, "task_id": mine["task_id"], "field": "run",
                         "compute": mine["state"], "stats": "absent"})
            continue
        for field, read in FIELDS:
            value = mine[field]
            other = read(theirs)
            if value in NOT_A_VALUE and other is None:
                continue
            if value == "" and other == "":
                continue
            if value != shown(other):
                rows.append({"run_id": run_id, "task_id": mine["task_id"], "field": field,
                             "compute": value, "stats": shown(other)})
    with open(os.path.join(out, "crosscheck.csv"), "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=["run_id", "task_id", "field", "compute", "stats"],
                           lineterminator="\n")
        w.writeheader()
        w.writerows(rows)
    print("runs %d, differences %d -> %s" % (len(runs), len(rows), os.path.join(out, "crosscheck.csv")))


if __name__ == "__main__":
    main()
