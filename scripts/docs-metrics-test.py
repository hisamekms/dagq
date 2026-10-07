#!/usr/bin/env python3
"""Run scripts/docs-metrics.py on fixtures and exit 1 when a value is wrong.

M1 and M2 read a small Git repository made here with fixed committer dates
(a first-parent merge, a commit on the period's end). M3 reads
scripts/docs-metrics-fixtures/stats.json. M4 reads the transcripts under
scripts/docs-metrics-fixtures/projects/: run A has two worker conversations,
a subagent, a review, a recovery job and a record on the period's end; run B's
worker starts just before the period; run C's worker reads no src/; run D's
model is not Claude; an inbox dir, a temporary queue's dir and a run dir that
is not a worktree are left out; a grep that reads a pipe is not a grep call.
M5 reads a second Git repository made here (a merge base, a main and a head
commit) and events written as two `dagq events --full` files: one event per
type, an event with hunks of two types, one without merge_base, one whose main
is not an object, one with a path the three ends merge cleanly (partial) and
one with only such a path (irreproducible), added and deleted files, an event
mixing docs/ and src/, an event id in both files and the period's two ends.
"""

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "docs-metrics.py")
FIXTURES = os.path.join(HERE, "docs-metrics-fixtures")

failures = []


def check(name, got, want):
    if got != want:
        failures.append(f"{name}: got {got!r}, want {want!r}")


def git(repo, *args, at=None):
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@localhost",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@localhost")
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    if at:
        env["GIT_AUTHOR_DATE"] = env["GIT_COMMITTER_DATE"] = at
    subprocess.run(["git", "-C", repo, "-c", "commit.gpgsign=false", *args], check=True,
                   capture_output=True, env=env)


def write(repo, path, text):
    full = os.path.join(repo, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w") as f:
        f.write(text)


def commit(repo, at, message):
    git(repo, "add", "-A")
    git(repo, "commit", "--quiet", "--no-verify", "-m", message, at=at)


def make_repo(repo):
    git(repo, "init", "--quiet", "-b", "main")
    # c1, before the M2 period: 100 + 31,000 bytes (one over 30 KiB).
    write(repo, "docs/design/a.md", "a\n" * 50)
    write(repo, "docs/design/persistence.md", "p" * 30999 + "\n")
    write(repo, "src/x.rs", "fn x() {}\n")
    commit(repo, "2026-09-30T12:00:00Z", "c1")
    # A side branch merged with --no-ff: only the merge is a landing.
    git(repo, "checkout", "--quiet", "-b", "side")
    write(repo, "docs/design/b.md", "b\n" * 10)
    commit(repo, "2026-09-30T13:00:00Z", "side")
    git(repo, "checkout", "--quiet", "main")
    # c2: a.md +3 -1 (104 bytes), persistence.md +1 (31,002 bytes), plans not counted.
    write(repo, "docs/design/a.md", "a\n" * 49 + "x\n" * 3)
    write(repo, "docs/design/persistence.md", "p" * 30999 + "\nq\n")
    write(repo, "docs/plans/p.md", "plan\n")
    commit(repo, "2026-10-01T05:00:00Z", "c2")
    git(repo, "merge", "--quiet", "--no-ff", "-m", "merge side", "side", at="2026-10-01T10:00:00Z")
    # c3: src only, the last second of 10-01.
    write(repo, "src/x.rs", "fn x() { 1 }\n")
    commit(repo, "2026-10-01T23:59:59Z", "c3")
    # c4 on the M2 period's end: not counted.
    write(repo, "docs/design/supervisor-lifecycle/stats.md", "s\n" * 5)
    commit(repo, "2026-10-02T06:00:00Z", "c4")


def run(*args):
    out = subprocess.run([sys.executable, SCRIPT, *args, "--format", "json"],
                         capture_output=True, text=True)
    if out.returncode != 0:
        failures.append(f"docs-metrics.py {' '.join(args)} exited {out.returncode}: {out.stderr}")
        return None
    return json.loads(out.stdout)


def check_m1_m2(repo):
    out = run("--repo", repo, "--since", "2026-09-30", "--until", "2026-10-03", "--metrics", "m1")
    if out:
        days = [(d["day"], d["bytes"], d["diff"], d["docs"], d["over_30kib"]) for d in out["m1"]["days"]]
        check("m1 days", days, [
            ("2026-09-30", 0, None, 0, 0),  # nothing on main before 09-30 00:00
            ("2026-10-01", 31100, 31100, 2, 1),  # c1
            ("2026-10-02", 31126, 26, 3, 1),  # c2, the merge and c3; not c4 (06:00)
        ])
        check("m1 growth", (out["m1"]["growth"], out["m1"]["growth_per_day"]), (31126, 15563))
        check("m2 not asked", out["m2"], None)
    out = run("--repo", repo, "--since", "2026-10-01T00:00:00Z", "--until", "2026-10-02T06:00:00Z",
              "--metrics", "m2")
    if out:
        check("m2", out["m2"], {
            "landings": 3,  # c2, the merge, c3 (not c1 before, not c4 on the end, not the side commit)
            "design_landings": 2,  # c2, the merge
            "design_share": 0.6667,
            "top_doc_landings": 1,  # c2 changes persistence.md
            "top_doc_share": 0.3333,
            "lines_added": 14,  # 3 + 1 + 10
            "lines_deleted": 1,
            "lines_changed": 15,
        })
    out = run("--repo", repo, "--since", "2026-10-02T06:00:00Z", "--until", "2026-10-03", "--metrics", "m2")
    if out:
        check("m2 the end is the next period's start", (out["m2"]["landings"], out["m2"]["top_doc_landings"]),
              (1, 1))


def check_m3_m4():
    projects = os.path.join(FIXTURES, "projects")
    out = run("--since", "2026-10-01", "--until", "2026-10-08", "--metrics", "m3,m4",
              "--stats", os.path.join(FIXTURES, "stats.json"), "--claude-projects", projects)
    if not out:
        return
    check("m3", out["m3"], {
        "conflicts": 10, "docs_conflicts": 8, "docs_share": 0.8, "hotspot_files": 3,
        "docs_hotspot_files": 2, "deferrals": 4, "deferral_secs": 7200, "deferral_hours": 2.0,
        "expired": 1, "expired_secs": 3600,
    })
    m4 = out["m4"]
    # wa1, wa2, wb (a result in the period though it starts before), wc; ra and
    # rc (no model record, still counted).
    check("m4 conversations", m4["conversations"], {"worker": 4, "review": 2, "all": 6})
    # inbox, the temporary queue and the run dir; recovery; the gpt-5 worker.
    check("m4 skipped", m4["skipped"], {"dirs": 3, "not_worker_or_review": 1, "not_claude": 1})
    # 16 results: the one on 10-08 00:00 and wb's src read before 10-01 are out.
    # wa2's two-line `cd ...` then `rg ... docs/design/persistence.md \ src/c.rs`
    # is a grep call, a top-document grep and a src/ read.
    # wc's `git log | grep docs/design/persistence.md` points to docs/ but is not
    # a grep call (it reads a pipe) nor a grep of a top document.
    check("m4 all", m4["all"], {
        "tool_results": 16, "chars": 1010, "docs_chars": 560, "docs_share": 0.5545,
        "src_chars": 183, "src_share": 0.1812, "whole_top_doc_reads": 1, "grep_calls": 9,
        "ranged_reads": 1,
        "median_chars": 35.0,  # even: (30 + 40) / 2
        "p90_chars": 200,  # ceil(16 * 0.9) = 15th
    })
    check("m4 worker quantiles", (m4["worker"]["tool_results"], m4["worker"]["median_chars"],
                                  m4["worker"]["p90_chars"]), (14, 25.0, 100))  # (20 + 30) / 2; ceil(12.6) = 13th
    check("m4 review", (m4["review"]["tool_results"], m4["review"]["docs_chars"], m4["review"]["grep_calls"],
                        m4["review"]["median_chars"], m4["review"]["p90_chars"]),
          (2, 0, 1, 140.0, 200))  # even: (80 + 200) / 2; ceil(1.8) = 2nd
    # (a) the two-line rg 13, Grep docs/design 40, rg docs/design/supervisor-lifecycle
    # 70, Grep glob **/provider-lifecycle.md 80; not grep docs/plans,
    # -e ... docs/plans/x.md, Grep docs or `git log | grep docs/design/persistence.md`.
    check("m4 top doc grep", m4["top_doc_grep"],
          {"results": 4, "median_chars": 55.0, "p90_chars": 80})  # even: (40 + 70) / 2; ceil(3.6) = 4th
    # (b) run A: 100 + 60 + 10 (subagent) + 13 + 1000 (after the period, still run A's);
    # run C: 0; run B's worker starts before the period; run D is not Claude.
    check("m4 worker src per run", m4["worker_src_per_run"],
          {"runs": 2, "zero_runs": 1, "median_chars": 591.5, "p90_chars": 1183})

    # The period's start is inclusive: from wa1's first record on, run B is out.
    out = run("--since", "2026-10-01T00:00:05Z", "--until", "2026-10-01T00:00:06Z", "--metrics", "m4",
              "--claude-projects", projects)
    if out:
        check("m4 one second", (out["m4"]["all"]["tool_results"], out["m4"]["all"]["chars"]), (2, 360))
        check("m4 one second runs", out["m4"]["worker_src_per_run"]["runs"], 0)


def make_conflicts(repo):
    """A merge base, and main and head on top of it; returns (base, main, head)."""
    git(repo, "init", "--quiet", "-b", "main")
    head_of = lambda: subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True,
                                     text=True, check=True).stdout.strip()
    clean = "".join(f"line {n}\n" for n in range(1, 13))
    base_files = {
        "docs/date.md": "---\nid: x\nupdated: 2026-10-01\n---\n\n# T\n\nbody\n",
        "docs/append.md": "# T\n\n| a | b |\n| --- | --- |\n| 1 | x |\n",
        "docs/list.md": "# T\n\n- one\n- two\n- three\n\nend\n",
        "docs/para.md": "# T\n\nThis is a paragraph.\nSecond line.\n\nend\n",
        "docs/table.md": "| a | b |\n| --- | --- |\n| 1 | x |\n| 2 | y |\n",
        "docs/gone.md": "gone\n",
        "docs/clean.md": clean,
        "src/x.rs": "fn x() {}\n",
    }
    for path, text in base_files.items():
        write(repo, path, text)
    commit(repo, "2026-09-30T00:00:00Z", "base")
    base = head_of()
    sides = {}
    for side in ("main", "head"):
        git(repo, "checkout", "--quiet", "-B", side, base)
        write(repo, "docs/date.md", base_files["docs/date.md"].replace("10-01", "10-02" if side == "main" else "10-03"))
        write(repo, "docs/append.md", base_files["docs/append.md"] + f"| 2 | {side} |\n")
        write(repo, "docs/list.md", base_files["docs/list.md"].replace("- two", f"- two {side}"))
        write(repo, "docs/para.md", base_files["docs/para.md"].replace("a paragraph", f"the {side} paragraph"))
        write(repo, "docs/table.md", base_files["docs/table.md"].replace("| 1 | x |", f"| 1 | {side} |"))
        write(repo, "docs/new.md", f"new on {side}\n")
        write(repo, "src/x.rs", f"fn x() {{ {side} }}\n")
        if side == "main":
            os.remove(os.path.join(repo, "docs/gone.md"))
            write(repo, "docs/clean.md", clean.replace("line 1\n", "line one\n"))
        else:
            write(repo, "docs/gone.md", "kept on head\n")
            write(repo, "docs/clean.md", clean.replace("line 12\n", "line twelve\n"))
        commit(repo, "2026-09-30T01:00:00Z", side)
        sides[side] = head_of()
    return base, sides["main"], sides["head"]


def check_m5(base, repo):
    commits = make_conflicts(repo)
    merge_base, main, head = commits
    missing = "0" * 40

    def event(id, kind, at, paths, with_base=True, main=main, **payload):
        payload = {"code": "rebase_conflict", "main": main, "head": head, "conflicts": paths, **payload}
        if with_base:
            payload["merge_base"] = merge_base
        return {"id": id, "run_id": f"run-{id}", "kind": kind, "created_at": at, "payload": payload}

    cp, lrf, idf = "conflict_precheck", "landing_recheck_failed", "integration_deferred"
    first = [
        event(1, cp, "2026-10-01T01:00:00.000Z", ["docs/date.md"]),  # type 1
        event(2, lrf, "2026-10-01T02:00:00.000Z", ["docs/append.md"], with_base=False),  # 2, git merge-base
        event(3, idf, "2026-10-01T03:00:00.000Z", ["docs/list.md", "docs/date.md"], with_base=False),  # 3 over 1
        event(4, cp, "2026-10-01T00:00:00.000Z", ["docs/para.md"]),  # 4, on the period's start
        event(5, lrf, "2026-10-01T05:00:00.000Z", ["docs/table.md"], with_base=False),  # 5
        event(6, idf, "2026-10-01T06:00:00.000Z", ["docs/new.md", "docs/gone.md"], with_base=False),  # 5, 5
        event(7, cp, "2026-10-02T07:00:00.000Z", ["docs/para.md", "docs/clean.md"]),  # 4, partial
        event(8, cp, "2026-10-02T08:00:00.000Z", ["docs/clean.md"]),  # no hunk: irreproducible
        event(9, lrf, "2026-10-02T09:00:00.000Z", ["docs/date.md"], with_base=False, main=missing),  # no object
        # Withdrawn requests and other deferrals carry no conflicts.
        event(10, cp, "2026-10-02T10:00:00.000Z", [], unsent=True),
        {"id": 11, "kind": idf, "created_at": "2026-10-02T11:00:00.000Z", "payload": {"code": "verification_failed"}},
    ]
    second = [
        event(1, cp, "2026-10-01T01:00:00.000Z", ["docs/date.md"]),  # the same id again
        event(12, cp, "2026-10-02T12:00:00.000Z", ["docs/date.md", "src/x.rs"]),  # mixed
        event(13, cp, "2026-09-30T23:59:59.999Z", ["docs/date.md"]),  # before the period
        event(14, lrf, "2026-10-03T00:00:00.000Z", ["docs/date.md"]),  # on the period's end
        {"id": 15, "kind": "run_started", "created_at": "2026-10-02T00:00:00.000Z", "payload": {}},
    ]
    files = []
    for n, events in enumerate((first, second)):
        files.append(os.path.join(base, f"events-{n}.json"))
        with open(files[-1], "w") as f:
            json.dump({"cursor": 99, "events": events} if n == 0 else events, f)
    out = run("--repo", repo, "--since", "2026-10-01", "--until", "2026-10-03", "--metrics", "m5",
              "--events", *files)
    if not out:
        return None
    m = out["m5"]
    check("m5 events", (m["conflict_events"], m["mixed_events"], m["docs_events"], m["docs_events_per_day"]),
          (10, 1, {cp: 4, lrf: 3, idf: 2, "total": 9}, 4.5))
    check("m5 reproduced", (m["classified"], m["partial"], m["irreproducible"], m["paths_irreproducible"]),
          (7, 1, 2, 2))
    # Over the 7 classified events, not the 9 docs-only ones.
    check("m5 types", m["types"], {
        "1_frontmatter_date": {"events": 1, "share": 0.1429},
        "2_append": {"events": 1, "share": 0.1429},
        "3_list_item": {"events": 1, "share": 0.1429},
        "4_paragraph": {"events": 2, "share": 0.2857},
        "5_table_or_file": {"events": 2, "share": 0.2857},
    })
    check("m5 hunks", m["hunks"], {"total": 9, "1_frontmatter_date": 2, "2_append": 1, "3_list_item": 1,
                                   "4_paragraph": 2, "5_table_or_file": 3})
    # The event's type is the heaviest hunk's: 4 over 5, 3 over 2. A path at
    # none of the three commits is an irreproducible path; a missing head
    # makes the event irreproducible.
    later = [
        event(20, cp, "2026-10-05T01:00:00.000Z", ["docs/table.md", "docs/para.md"]),
        event(21, cp, "2026-10-05T02:00:00.000Z", ["docs/append.md", "docs/list.md"]),
        event(22, cp, "2026-10-05T03:00:00.000Z", ["docs/nowhere.md", "docs/date.md"]),
        {**event(23, cp, "2026-10-05T04:00:00.000Z", ["docs/date.md"]),
         "payload": {"main": main, "head": missing, "merge_base": merge_base, "conflicts": ["docs/date.md"]}},
    ]
    files.append(os.path.join(base, "events-later.json"))
    with open(files[-1], "w") as f:
        json.dump({"events": later}, f)
    out = run("--repo", repo, "--since", "2026-10-05", "--until", "2026-10-06", "--metrics", "m5",
              "--events", *files)
    if out:
        m = out["m5"]
        check("m5 later reproduced", (m["docs_events"]["total"], m["classified"], m["partial"],
                                      m["irreproducible"], m["paths_irreproducible"]), (4, 3, 1, 1, 1))
        check("m5 later types", {k: v["events"] for k, v in m["types"].items()},
              {"1_frontmatter_date": 1, "2_append": 0, "3_list_item": 1, "4_paragraph": 1, "5_table_or_file": 0})
        check("m5 later hunks", m["hunks"], {"total": 5, "1_frontmatter_date": 1, "2_append": 1,
                                             "3_list_item": 1, "4_paragraph": 1, "5_table_or_file": 1})
    files.pop()
    # Without --events M5 is not counted.
    out = run("--repo", repo, "--since", "2026-10-01", "--until", "2026-10-03", "--metrics", "m5")
    if out:
        check("m5 without events", out["m5"], None)
    return files


def check_hunk_rules():
    spec = importlib.util.spec_from_file_location("docs_metrics", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    classify = module.classify_hunk
    check("hunk date", classify(["updated: 2026-10-02"], ["updated: 2026-10-01", ""], ["last_verified: x"]), 1)
    check("hunk dates added on both sides", classify(["updated: 2"], [], ["updated: 3"]), 1)
    check("hunk append of list items", classify(["- a"], [], ["- b"]), 2)
    check("hunk table", classify(["| 1 | a |"], ["| 1 | x |"], ["| 1 | b |"]), 5)
    check("hunk list", classify(["  1. a", "- b"], ["* c"], ["- d"]), 3)
    check("hunk date and body", classify(["updated: 1", "text"], ["updated: 0"], ["updated: 2"]), 4)
    check("hunk deleted on one side", classify([], ["- a"], ["- b"]), 3)
    check("hunk paragraph", classify(["a"], ["b"], ["c"]), 4)


def main():
    base = tempfile.mkdtemp(prefix="docs-metrics-test.")
    try:
        repo = os.path.join(base, "repo")
        os.makedirs(repo)
        make_repo(repo)
        check_m1_m2(repo)
        check_m3_m4()
        check_hunk_rules()
        m5_repo = os.path.join(base, "m5")
        os.makedirs(m5_repo)
        events = check_m5(base, m5_repo) or []
        out = subprocess.run([sys.executable, SCRIPT, "--repo", repo, "--since", "2026-10-01",
                              "--until", "2026-10-03", "--stats", os.path.join(FIXTURES, "stats.json"),
                              "--claude-projects", os.path.join(FIXTURES, "projects")],
                             capture_output=True, text=True)
        check("markdown exit", out.returncode, 0)
        m5 = subprocess.run([sys.executable, SCRIPT, "--repo", m5_repo, "--since", "2026-10-01",
                             "--until", "2026-10-03", "--metrics", "m5", "--events", *events],
                            capture_output=True, text=True)
        check("markdown has M5", "## M5" in m5.stdout and "| 4_paragraph | 2 | 0.2857 | 2 |" in m5.stdout, True)
        for heading in ("## M1", "## M2", "## M3", "## M4", "(a) grep", "(b) worker"):
            check(f"markdown has {heading}", heading in out.stdout, True)
        # --input prints a saved JSON output as the same Markdown.
        saved = os.path.join(base, "saved.json")
        with open(saved, "w") as f:
            json.dump(run("--repo", repo, "--since", "2026-10-01", "--until", "2026-10-03",
                          "--stats", os.path.join(FIXTURES, "stats.json"),
                          "--claude-projects", os.path.join(FIXTURES, "projects")), f)
        again = subprocess.run([sys.executable, SCRIPT, "--input", saved], capture_output=True, text=True)
        check("--input renders the same Markdown", again.stdout, out.stdout)
    finally:
        shutil.rmtree(base, ignore_errors=True)

    if failures:
        for f in failures:
            print("FAIL", f)
        sys.exit(1)
    print("ok: docs-metrics.py passed every check")


if __name__ == "__main__":
    main()
