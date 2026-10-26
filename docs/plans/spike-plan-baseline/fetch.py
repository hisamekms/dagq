#!/usr/bin/env python3
"""Fetch run events into a local cache with the read-only `dagq events`.

  python3 -I fetch.py --out DIR (--goal N | --task N)... [--kind K]... [--since T] [--until T]

Pages with `--after <cursor>` and `--limit` so that each answer stays under
the queue service's response limit; writes DIR/<name>.json as one list of
events (the `--full` form). Reads only; changes no queue state.
"""

import argparse
import json
import os
import subprocess

PAGE = 1000


def events(args, page=PAGE):
    """Every event `dagq events --all --full ARGS` returns, oldest first."""
    out, after = [], 0
    while True:
        cmd = ["dagq", "events", "--all", "--full", "--limit", str(page),
               "--after", str(after)] + list(args)
        res = subprocess.run(cmd, capture_output=True, text=True)
        if res.returncode != 0:
            if "response_too_large" in res.stdout + res.stderr and page > 50:
                page //= 4
                continue
            raise SystemExit(f"{' '.join(cmd)}: {res.stdout}{res.stderr}")
        got = json.loads(res.stdout)["events"]
        out.extend(got)
        if len(got) < page:
            return out
        after = got[-1]["id"]


def cached(out_dir, name, args):
    path = os.path.join(out_dir, f"{name}.json")
    if os.path.exists(path):
        with open(path) as f:
            return json.load(f)
    evs = events(args)
    with open(path, "w") as f:
        json.dump(evs, f)
    return evs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--goal", type=int, action="append", default=[])
    ap.add_argument("--task", type=int, action="append", default=[])
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    for g in a.goal:
        print(f"goal {g}: {len(cached(a.out, f'goal-{g}', ['--goal', str(g)]))}")
    for t in a.task:
        print(f"task {t}: {len(cached(a.out, f'task-{t}', ['--task', str(t)]))}")


if __name__ == "__main__":
    main()
