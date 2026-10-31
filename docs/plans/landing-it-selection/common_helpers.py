#!/usr/bin/env python3
"""Applies scripts/landing-it.py's choice for the changed helpers under
tests/common to main's landings that touched them, and counts how many of
them ran every IT by the common files before and after the helpers left the
common files (docs/plans/landing-it-selection.md, 「tests/commonのhelperの選び方の測定」).

Usage: python3 common_helpers.py SINCE [--repo REPO] [--ref NAME=REV ...]
  SINCE  the start of the period (UTC, 2026-10-17T04:30:51Z); the landings
         are main's commits after it, each squash commit one landing
  --ref  a diff to add as a row: the change of REV from its merge base with
         main (a run that did not land)

Every row is measured on the tree at REPO (the checked-out one): the helpers'
users and the #[test] counts are today's, not those of the landing's commit.
Only the common files are compared; the threshold, the broad entries and the
table's age need the table of the day and are not applied. It reads git
only, so it gives the same result on every run.
"""
import argparse
import importlib.util
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load_landing_it(repo):
    spec = importlib.util.spec_from_file_location(
        "landing_it", os.path.join(repo, "scripts", "landing-it.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def git(repo, *args):
    return subprocess.run(["git", "-C", repo] + list(args), check=True,
                          capture_output=True, text=True).stdout


def landings(repo, since):
    out = git(repo, "log", "main", "--since=" + since, "--no-renames",
              "--format=@%h %cI", "--name-only")
    rows, cur = [], None
    for line in out.splitlines():
        if line.startswith("@"):
            sha, when = line[1:].split(" ", 1)
            cur = {"commit": sha, "when": when, "diff": []}
            rows.append(cur)
        elif line and cur is not None:
            cur["diff"].append(line)
    return rows


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("since")
    parser.add_argument("--repo", default=os.path.normpath(os.path.join(HERE, "..", "..", "..")))
    parser.add_argument("--ref", action="append", default=[])
    args = parser.parse_args()
    li = load_landing_it(args.repo)
    old_common = li.COMMON_FILES + [li.HELPER_DIR + "/**"]
    rows = landings(args.repo, args.since)
    for spec in args.ref:
        name, rev = spec.split("=", 1)
        base = git(args.repo, "merge-base", "main", rev).strip()
        diff = git(args.repo, "diff", "--name-only", "--no-renames", base, rev).split()
        rows.append({"commit": name, "when": "", "diff": diff, "ref": True})
    tree = li.test_tree(args.repo)
    limit = li.BROAD_SHARE * tree["total"]
    print("IT (#[test]) %d, BROAD_SHARE %g -> more than %.1f runs every IT"
          % (tree["total"], li.BROAD_SHARE, limit))
    print("| commit | 触れた helper | 使う module の数 | その IT の本数 | 新しい規則 | 選ぶ IT の本数 |")
    print("|---|---|---|---|---|---|")
    before = after = touching = 0
    for row in rows:
        diff = row["diff"]
        other = [p for p in diff if any(li.glob_match(p, g) for g in li.COMMON_FILES)]
        old_full = any(any(li.glob_match(p, g) for g in old_common) for p in diff)
        helpers = [p for p in diff if p.startswith(li.HELPER_DIR + "/")]
        landed = not row.get("ref")
        before += landed and old_full
        if not helpers:
            after += bool(other)
            continue
        touching += landed
        full = bool(other)
        cells, chosen = [], set()
        for p in helpers:
            h = li.helper_of(p)
            users = li.helper_users(tree, {h}) if h else set()
            n = sum(tree["modules"][k]["tests"] for k in users)
            if h is None or n > limit:
                full = True
            else:
                chosen |= users
            cells.append((os.path.basename(p), len(users), n))
        after += landed and full
        picked = sum(tree["modules"][k]["tests"] for k in chosen)
        verdict = "全部流す" if full else "選ぶ"
        if other:
            verdict += "（共通のファイル %s）" % "・".join("`%s`" % p for p in other)
        print("| `%s` | %s | %s | %s | %s | %s |" % (
            row["commit"],
            "・".join("`%s`" % c[0] for c in cells),
            "・".join(str(c[1]) for c in cells),
            "・".join(str(c[2]) for c in cells),
            verdict,
            "%d（全部）" % tree["total"] if full else str(picked),
        ))
    print()
    print("landings %d, touching tests/common %d (the --ref rows not counted)"
          % (sum(not r.get("ref") for r in rows), touching))
    print("every IT by the common files: before %d, after %d" % (before, after))


if __name__ == "__main__":
    sys.exit(main())
