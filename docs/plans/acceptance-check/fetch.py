#!/usr/bin/env python3
"""Snapshot the production queue for the acceptance-check baseline (task 1422)
and the after comparison (task 1423).

Reads only: `dagq events` / `dagq show` / `dagq stats` / `dagq marks` /
`dagq status` (all read-only commands) and git. Never opens the queue DB.

  python3 fetch.py --since S --until E --cutoff C [--out DIR] [--watch-task ID ...]

Runs: every run with a `review_finished` or `review_failed` created in
[S, E) (the compute step decides which of them are reviewed runs: the run's
first `review_finished` with a verdict falls in an interval). Everything a
value is computed from is cut at C (created_at < C). Also the queue-wide
`supervisor_started` / `update_installed` / `update_failed` events and the
`approve_update` / `update_failed` asks (to find when a supervisor first ran
a binary holding a watched task's landing, task 1423), with
normalized/versions.jsonl saying, from git, which of those versions hold
each watched task's landing commit.

Writes DIR/normalized/ (compared, read by compute) and DIR/reference/
(raw outputs holding the current state; read only by crosscheck).
"""

import argparse
import gzip
import json
import os
import re
import subprocess
import sys

PAGE = 1000


def run(cmd, cwd=None):
    out = subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True)
    return out.stdout


def reference(cmd):
    """A reference output; a failing read is written down, not fatal (task 1423)."""
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else json.dumps(
        {"command": cmd, "exit": r.returncode, "stdout": r.stdout, "stderr": r.stderr}, indent=1)


def dagq_json(args):
    return json.loads(run(["dagq"] + args))


def events(args):
    """Every event of `dagq events --full <args>`, paging with --after."""
    after = 0
    found = []
    while True:
        page = dagq_json(
            ["events", "--full", "--limit", str(PAGE), "--after", str(after)] + args
        )["events"]
        if not page:
            return found
        found.extend(page)
        after = max(e["id"] for e in page)


FIELDS = ("id", "kind", "run_id", "task_id", "goal_id", "created_at", "actor", "payload")


def normal(event):
    return json.dumps(
        {k: event.get(k) for k in FIELDS}, sort_keys=True, ensure_ascii=False
    )


def write_lines(path, lines):
    with open(path, "w", encoding="utf-8") as f:
        for line in lines:
            f.write(line + "\n")


def write_gz(path, text):
    """The large raw outputs, gzipped (mtime 0) to keep the repository small."""
    with open(path, "wb") as raw:
        with gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as f:
            f.write(text.encode("utf-8"))


def items(acceptance):
    """The number of `(digits)` in the acceptance, 1 when there is none."""
    if acceptance is None:
        return None
    return max(1, len(re.findall(r"\(\d+\)", acceptance)))


def item_numbers(acceptance):
    """The distinct numbers of the `(digits)` in the acceptance (task 1423)."""
    if acceptance is None:
        return None
    return sorted({int(n) for n in re.findall(r"\((\d+)\)", acceptance)})


UPDATE_ASKS = ("approve_update", "update_failed")


def version_commit(repo, version):
    """The commit of a dagq version: the sha after `+`, else the tag v<version>."""
    if not version:
        return None
    if "+" in version:
        return version.split("+", 1)[1]
    try:
        return run(["git", "-C", repo, "rev-parse", "v%s^{commit}" % version]).strip()
    except subprocess.CalledProcessError:
        return None


def holds(repo, ancestor, commit):
    """True when `ancestor` is an ancestor of (or is) `commit`; None when git
    does not know one of them."""
    r = subprocess.run(["git", "-C", repo, "merge-base", "--is-ancestor", ancestor, commit],
                       capture_output=True)
    return {0: True, 1: False}.get(r.returncode)


def task_at_claim(task_id, task_events):
    """The task's change and acceptance at its first run_claimed: the
    current `show --full` values with every task_edited created at or after
    that claim rolled back to its `from`, newest first. The task_edited
    events are read up to now (not C) so later edits are undone too."""
    shown = dagq_json(["show", str(task_id), "--full"])
    values = {"change": shown["task"].get("change"), "acceptance": shown["task"].get("acceptance")}
    claims = [e["created_at"] for e in task_events if e["kind"] == "run_claimed"]
    missing = []
    if claims:
        first_claim = min(claims)
        edits = events(["--all", "--task", str(task_id), "--kind", "task_edited"])
        for edit in sorted(edits, key=lambda e: e["id"], reverse=True):
            if edit["created_at"] < first_claim:
                continue
            before = edit["payload"].get("from") or {}
            after = edit["payload"].get("to") or {}
            for field in ("change", "acceptance"):
                if field in before:
                    values[field] = before[field]
                elif field in after:
                    missing.append(field)
    row = {
        "task_id": task_id,
        "change": None if "change" in missing else values["change"],
        "items": None if "acceptance" in missing else items(values["acceptance"]),
        "item_numbers": None if "acceptance" in missing else item_numbers(values["acceptance"]),
        "missing": sorted(set(missing)),
    }
    return row, shown


def landed_commit(repo, commit):
    """Additions, deletions and files of a landing commit against its first parent."""
    try:
        out = run(
            ["git", "-C", repo, "show", "--numstat", "--no-renames",
             "--diff-merges=first-parent", "--format=", commit]
        )
    except subprocess.CalledProcessError:
        return {"commit": commit, "found": False, "added": None, "deleted": None, "files": []}
    added = deleted = 0
    files = []
    for line in out.splitlines():
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        a, d, path = parts
        added += int(a) if a.isdigit() else 0
        deleted += int(d) if d.isdigit() else 0
        files.append(path)
    return {"commit": commit, "found": True, "added": added, "deleted": deleted, "files": sorted(files)}


def areas_table(repo, cutoff):
    """The [areas] of dagq.toml at the last main commit before C."""
    commit = run(["git", "-C", repo, "rev-list", "-1", "--before=" + cutoff, "main"]).strip()
    toml = run(["git", "-C", repo, "show", commit + ":dagq.toml"])
    lines, inside = [], False
    for line in toml.splitlines():
        if line.startswith("["):
            inside = line.strip() == "[areas]"
            continue
        if inside and line.strip() and not line.lstrip().startswith("#"):
            lines.append(line)
    return "# main commit: %s (git rev-list -1 --before=%s main)\n[areas]\n%s\n" % (
        commit, cutoff, "\n".join(lines))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--since", required=True, help="interval start S (UTC, included)")
    p.add_argument("--until", required=True, help="interval end E (UTC, excluded)")
    p.add_argument("--cutoff", required=True, help="observation cutoff C (UTC, excluded)")
    p.add_argument("--stats-since", default=None, help="start of the reference stats window")
    p.add_argument("--watch-task", action="append", default=[], type=int,
                   help="a task whose events (to C) are also kept, e.g. 1420")
    p.add_argument("--areas-from", default=None,
                   help="copy this areas.toml (a baseline's) instead of reading dagq.toml at C")
    p.add_argument("--repo", default=".")
    p.add_argument("--out", default=None)
    a = p.parse_args()
    if not a.until <= a.cutoff:
        sys.exit("the cutoff C must not be before the interval end E")
    here = os.path.dirname(os.path.abspath(__file__))
    out = a.out or os.path.join(here, "snapshot", a.cutoff)
    norm = os.path.join(out, "normalized")
    ref = os.path.join(out, "reference")
    os.makedirs(norm, exist_ok=True)
    os.makedirs(ref, exist_ok=True)

    reviewed = events(["--kind", "review_finished", "--kind", "review_failed",
                       "--since", a.since, "--until", a.until])
    run_ids = sorted({e["run_id"] for e in reviewed if e.get("run_id")})
    task_ids = sorted({e["task_id"] for e in reviewed if e.get("task_id") is not None})
    task_ids = sorted(set(task_ids) | set(a.watch_task))

    by_id = {}
    for run_id in run_ids:
        for e in events(["--all", "--run", run_id, "--until", a.cutoff]):
            by_id[e["id"]] = e
    per_task = {}
    for task_id in task_ids:
        got = events(["--all", "--task", str(task_id), "--until", a.cutoff])
        per_task[task_id] = got
        for e in got:
            by_id[e["id"]] = e
    for e in events(["--all", "--kind", "mark_recorded", "--kind", "mark_retracted",
                     "--until", a.cutoff]):
        by_id[e["id"]] = e
    for e in events(["--all", "--kind", "supervisor_started", "--kind", "update_installed",
                     "--kind", "update_failed", "--until", a.cutoff]):
        by_id[e["id"]] = e
    for e in events(["--all", "--kind", "ask_opened", "--kind", "ask_answered", "--kind", "ask_closed",
                     "--until", a.cutoff]):
        if (e["payload"] or {}).get("kind") in UPDATE_ASKS:
            by_id[e["id"]] = e
    write_lines(os.path.join(norm, "events.jsonl"),
                [normal(by_id[i]) for i in sorted(by_id)])

    tasks, shows = [], {}
    for task_id in task_ids:
        row, shown = task_at_claim(task_id, per_task[task_id])
        tasks.append(json.dumps(row, sort_keys=True, ensure_ascii=False))
        shows[str(task_id)] = shown
    write_lines(os.path.join(norm, "tasks.jsonl"), tasks)

    commits = sorted({(by_id[i]["payload"].get("commit") or by_id[i]["payload"].get("result_commit"))
                      for i in by_id if by_id[i]["kind"] == "run_integrated"} - {None})
    write_lines(os.path.join(norm, "commits.jsonl"),
                [json.dumps(landed_commit(a.repo, c), sort_keys=True) for c in commits])
    landings = sorted({(e["payload"].get("commit") or e["payload"].get("result_commit"))
                       for e in by_id.values() if e["kind"] == "run_integrated"
                       and e.get("task_id") in a.watch_task} - {None})
    versions = {}
    for e in by_id.values():
        p = e["payload"] or {}
        if e["kind"] == "supervisor_started" and p.get("dagq_version"):
            versions[p["dagq_version"]] = None
        elif e["kind"] in ("update_installed", "update_failed") and p.get("commit"):
            versions[p["commit"]] = p["commit"]
    rows = []
    for v in sorted(versions):
        c = versions[v] or version_commit(a.repo, v)
        rows.append(json.dumps({"version": v, "commit": c,
                                "holds": {l: (None if c is None else holds(a.repo, l, c)) for l in landings}},
                               sort_keys=True))
    write_lines(os.path.join(norm, "versions.jsonl"), rows)
    if a.areas_from:
        with open(a.areas_from, encoding="utf-8") as f:
            areas = f.read()
    else:
        areas = areas_table(a.repo, a.cutoff)
    with open(os.path.join(norm, "areas.toml"), "w", encoding="utf-8") as f:
        f.write(areas)

    stats_since = a.stats_since or a.since
    write_gz(os.path.join(ref, "stats.json.gz"),
             run(["dagq", "stats", "--full", "--since", stats_since, "--until", a.cutoff]))
    write_gz(os.path.join(ref, "show.json.gz"),
             json.dumps(shows, sort_keys=True, ensure_ascii=False, indent=1))
    with open(os.path.join(ref, "marks.json"), "w", encoding="utf-8") as f:
        f.write(reference(["dagq", "marks"]))
    with open(os.path.join(ref, "status.json"), "w", encoding="utf-8") as f:
        f.write(reference(["dagq", "status"]))
    with open(os.path.join(ref, "args.json"), "w", encoding="utf-8") as f:
        json.dump(vars(a), f, sort_keys=True, indent=1)
    print("runs %d, tasks %d, events %d, commits %d -> %s"
          % (len(run_ids), len(task_ids), len(by_id), len(commits), out))


if __name__ == "__main__":
    main()
