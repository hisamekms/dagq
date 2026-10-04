#!/usr/bin/env python3
"""Run scripts/token-usage.py on the synthetic fixtures in scripts/token-usage-fixtures/.

Lays the fixtures out in a temporary home (a repository path with "." and "_",
a queue dir with "."), runs the script against a fake dagq that pages its
events, and exits 1 when a check fails.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "token-usage.py")
FIXTURES = os.path.join(HERE, "token-usage-fixtures")

failures = []


def check(name, got, want):
    if got != want:
        failures.append(f"{name}: got {got!r}, want {want!r}")


def encode(path):
    return "".join(c if c.isascii() and c.isalnum() else "-" for c in path)


def place(src, dst, subs):
    with open(os.path.join(FIXTURES, src)) as f:
        text = f.read()
    for key, value in subs.items():
        text = text.replace(key, value)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(dst, "w") as f:
        f.write(text)


def layout(base):
    repo = os.path.join(base, "github.com", "a_b", "repo.x")
    queue = os.path.join(base, ".local", "share", "dagq", "0123abcd")
    os.makedirs(repo)
    os.makedirs(queue)
    subs = {"{{REPO}}": repo, "{{QUEUE}}": queue}
    projects = os.path.join(base, "claude-projects")
    repo_dir = os.path.join(projects, encode(repo))
    worktree_dir = os.path.join(projects, encode(os.path.join(queue, "runs", "abc", "worktree")))
    smoke_dir = os.path.join(projects, encode(repo + "-smoke"))
    place("claude/review.jsonl", os.path.join(repo_dir, "s-review.jsonl"), subs)
    place("claude/review-sub.jsonl", os.path.join(repo_dir, "s-review", "subagents", "agent-1.jsonl"), subs)
    place("claude/human.jsonl", os.path.join(repo_dir, "s-human.jsonl"), subs)
    place("claude/worker.jsonl", os.path.join(worktree_dir, "s-worker.jsonl"), subs)
    # Outside the period: neither counted nor counted as excluded.
    place("claude/human-old.jsonl", os.path.join(repo_dir, "s-human-old.jsonl"), subs)
    resume_dir = os.path.join(projects, encode(os.path.join(queue, "runs", "def", "worktree")))
    place("claude/resume.jsonl", os.path.join(resume_dir, "s-resume.jsonl"), subs)
    place("claude/smoke.jsonl", os.path.join(smoke_dir, "s-smoke.jsonl"), subs)
    # A cwd that encodes to the repository's project dir but is another dir.
    place("claude/smoke.jsonl", os.path.join(repo_dir, "s-collide.jsonl"),
          {"{{REPO}}-smoke": repo[: -len("repo.x")] + "repo_x"})
    sessions = os.path.join(base, "codex-sessions")
    day = os.path.join(sessions, "2026", "10", "02")
    for name in ("root", "child", "old", "human"):
        place(f"codex/{name}.jsonl", os.path.join(day, f"rollout-{name}.jsonl"), subs)
    place("codex/old-outside.jsonl",
          os.path.join(sessions, "2026", "09", "20", "rollout-old-outside.jsonl"), subs)
    dagq = os.path.join(base, "dagq")
    shutil.copy(os.path.join(FIXTURES, "fake-dagq.py"), dagq)
    os.chmod(dagq, 0o755)
    return repo, queue, projects, sessions, dagq


def run(args, env):
    out = subprocess.run([sys.executable, SCRIPT, *args], capture_output=True, text=True, env=env)
    if out.returncode != 0:
        failures.append(f"token-usage.py {' '.join(args)} exited {out.returncode}: {out.stderr}")
        return None
    return out.stdout


def row(day, provider, actor, inp, cr, cc, out, messages, sessions):
    return {"day": day, "provider": provider, "actor": actor, "input": inp, "cache_read": cr,
            "cache_creation": cc, "output": out, "messages": messages, "sessions": sessions}


def main():
    base = tempfile.mkdtemp(prefix="token-usage-test.")
    try:
        repo, queue, projects, sessions, dagq = layout(base)
        log = os.path.join(base, "dagq.log")
        env = dict(os.environ, FAKE_DAGQ_EVENTS=os.path.join(FIXTURES, "events.json"),
                   FAKE_DAGQ_QUEUE=queue, FAKE_DAGQ_LOG=log)
        common = ["--repo", repo, "--since", "2026-10-01", "--until", "2026-10-03",
                  "--claude-projects", projects, "--codex-sessions", sessions, "--dagq", dagq]

        stdout = run([*common, "--queue-dir", queue, "--format", "json", "--page-size", "2"], env)
        if stdout is not None:
            out = json.loads(stdout)
            check("queue_hash", out["queue_hash"], "0123abcd")
            check("rows", out["rows"], [
                # 14:59:59Z is 10-01 JST; the duplicate (m1, q1) counts once.
                row("2026-10-01", "claude", "review", 10, 100, 1000, 1, 1, 1),
                # 15:00:00Z is 10-02 JST; the codex rows are below.
                row("2026-10-02", "claude", "review", 20, 200, 2000, 2, 1, 1),
                row("2026-10-02", "claude", "review+sub", 5, 0, 0, 5, 1, 1),
                row("2026-10-02", "claude", "worker", 7, 0, 0, 70, 1, 1),
                # (T1, resp1) twice and the child's (T2, resp_a) in two rollouts count once each;
                # the child thread goes to the root's actor; fresh input is input - cached.
                row("2026-10-02", "codex", "plan_review", 100, 50, 0, 15, 2, 1),
                # The old rollout falls back to token_count, skipping the repeated total.
                row("2026-10-02", "codex", "worker", 150, 0, 0, 5, 2, 1),
                # A new transcript that starts with a resume request in a run worktree.
                row("2026-10-03", "claude", "worker_resume", 3, 0, 0, 30, 1, 1),
            ])
            check("excluded_sessions", out["excluded_sessions"],
                  {"claude_prompt": 1, "claude_cwd": 1, "codex_prompt": 1})
            check("codex_fallback_rollouts", out["codex_fallback_rollouts"], 1)
            landings = {e["day"]: e["landings"] for e in out["per_landing"]}
            check("landings", landings, {"2026-10-01": 2, "2026-10-02": 3, "2026-10-03": 0})
            by_day = {e["day"]: e for e in out["per_landing"]}
            check("per landing 10-01", by_day["2026-10-01"]["claude"],
                  {"input": 5, "cache_read": 50, "cache_creation": 500, "output": 0})
            check("per landing 10-02 codex", by_day["2026-10-02"]["codex"],
                  {"input": 83, "cache_read": 17, "cache_creation": 0, "output": 7})
            check("per landing 10-03", by_day["2026-10-03"],
                  {"day": "2026-10-03", "landings": 0,
                   "claude": dict.fromkeys(("input", "cache_read", "cache_creation", "output")),
                   "codex": dict.fromkeys(("input", "cache_read", "cache_creation", "output"))})
            with open(log) as f:
                calls = [json.loads(line) for line in f]
            check("events pages", len(calls), 3)
            check("events window", calls[0][calls[0].index("--since") + 1: calls[0].index("--since") + 2]
                  + calls[0][calls[0].index("--until") + 1: calls[0].index("--until") + 2],
                  ["2026-09-30T15:00:00Z", "2026-10-03T15:00:00Z"])

        os.remove(log)
        stdout = run(common, env)  # the queue dir from `dagq locate`, as a table
        if stdout is not None:
            with open(log) as f:
                check("locate first", json.loads(f.readline()), ["locate"])
            check("table names the queue", f"queue 0123abcd ({queue})" in stdout, True)
            line = next((l for l in stdout.splitlines() if l.startswith("2026-10-03")
                         and "claude" not in l), "")
            check("table 10-03 per landing", line.split(), ["2026-10-03", "0"] + ["-"] * 8)
            check("table excluded", "claude 1, codex 1" in stdout, True)
    finally:
        shutil.rmtree(base, ignore_errors=True)

    if failures:
        for f in failures:
            print("FAIL", f)
        sys.exit(1)
    print("ok: token-usage.py passed every check")


if __name__ == "__main__":
    main()
