#!/usr/bin/env python3
"""Count Claude and Codex tokens per actor, JST day and landing for one dagq queue.

A read-only stopgap on the host (goal 95, task 1485): dagq's own record
(stats' sessions.by_kind.tokens) counts a session on the day it closes and
misses Claude subagents and Codex jobs. Remove this script and its mention in
the throughput-review skill once the permanent record (kpi / stats by
Execution, task 1494) lands.

Sources:
- Claude: <projects>/<project>/<session>.jsonl and <session>/subagents/*.jsonl,
  type=assistant message.usage, deduplicated by (message.id, requestId), put on
  the JST day of each record's timestamp. Subagents count as "<actor>+sub".
  A project counts when its name equals the encoded repository path, or starts
  with the encoded queue dir followed by "-" (run worktrees and planner cwds
  under the queue dir). The encoding is the runtime's encode_cwd: every
  character but ASCII letters and digits becomes "-".
- Codex: <sessions>/**/*.jsonl token_usage_record, deduplicated by
  (thread_id, response_id); a child thread's records go to the actor of its
  root (session_id). Rollouts without token_usage_record fall back to
  token_count's last_token_usage and are counted in the output.
- Landings: `dagq events --kind run_integrated`, paged until the period is read.

The actor comes from the session's first prompt. Sessions whose first prompt is
not one dagq starts (a person's session in the checkout, for example) are left
out of every total; only their number is printed.
"""

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
from collections import defaultdict

JST = dt.timezone(dt.timedelta(hours=9))
FIELDS = ("input", "cache_read", "cache_creation", "output")

# First-prompt prefixes of the actors dagq starts, checked on the prompt's head.
RULES = (
    ("You review run", "review"),
    ("You are the plan review", "plan_review"),
    ("You are the goal review", "goal_review"),
    ("You are a planner the dagq runtime", "runtime_planner"),
    ("You are a planner of the dagq", "planner"),
    ("You are the planner of the dagq", "planner"),
    ("You are the inbox", "inbox"),
    ("You are the observer", "observer"),
    ("You are the throughput review", "throughput_review"),
    ("You are dagq's recovery job", "recovery"),
    ("You triage run", "recovery"),  # the triage before the recovery job
    ("You are the maintainer", "maintainer"),  # retired, in older transcripts
    ("You are executing dagq task", "worker"),
)
RESUME_WORDS = ("resum", "revise", "answer to ask")
RESUME_PREFIX = "dagq:"  # the next turn or a needs_session request in a new transcript


def encode_cwd(path):
    """The runtime's encode_cwd (src/infrastructure/transcripts.rs)."""
    return "".join(c if c.isascii() and c.isalnum() else "-" for c in path)


def norm(path):
    return os.path.abspath(os.path.expanduser(path)).rstrip("/") or "/"


def classify(prompt, cwd, queue_dir):
    """The actor of a session from its first prompt, or None when dagq did not start it."""
    if not prompt:
        return None
    head = prompt.lstrip()[:300]
    for prefix, actor in RULES:
        if head.startswith(prefix):
            return actor
    runs = os.path.join(queue_dir, "runs") + "/"
    if cwd and cwd.startswith(runs) and (
        head.startswith(RESUME_PREFIX) or any(w in head.lower() for w in RESUME_WORDS)
    ):
        return "worker_resume"
    return None


def in_scope(cwd, repo, queue_dir):
    return cwd == repo or cwd.startswith(queue_dir + "/")


def jst_day(timestamp):
    if not timestamp:
        return None
    try:
        t = dt.datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
    except ValueError:
        return None
    if t.tzinfo is None:
        t = t.replace(tzinfo=dt.timezone.utc)
    return t.astimezone(JST).date().isoformat()


def read_jsonl(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    yield json.loads(line)
                except json.JSONDecodeError:
                    continue
    except OSError:
        return


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(
            b.get("text", "")
            for b in content
            if isinstance(b, dict) and b.get("type") in ("text", "input_text")
        )
    return ""


class Totals:
    def __init__(self, days):
        self.days = set(days)
        self.rows = defaultdict(lambda: {**{k: 0 for k in FIELDS}, "messages": 0, "sessions": set()})

    def touches(self, timestamps):
        """Whether any of the timestamps falls on a JST day of the period."""
        return any(jst_day(t) in self.days for t in timestamps)

    def add(self, provider, actor, day, session, usage):
        if day not in self.days:
            return
        row = self.rows[(day, provider, actor)]
        for k in FIELDS:
            row[k] += usage[k]
        row["messages"] += 1
        row["sessions"].add(session)


# Claude


def claude_first(records):
    """(cwd, first prompt) of a Claude transcript."""
    cwd = prompt = None
    for r in records:
        if cwd is None and isinstance(r.get("cwd"), str):
            cwd = r["cwd"]
        if prompt is None and r.get("type") == "user" and not r.get("isMeta"):
            text = text_of((r.get("message") or {}).get("content")).strip()
            if text and not text.startswith("<"):
                prompt = text
        if cwd is not None and prompt is not None:
            break
    return cwd, prompt


def claude_usage(records, seen):
    for r in records:
        if r.get("type") != "assistant":
            continue
        msg = r.get("message") or {}
        u = msg.get("usage")
        if not isinstance(u, dict):
            continue
        key = (msg.get("id"), r.get("requestId"))
        if key != (None, None):
            if key in seen:
                continue
            seen.add(key)
        yield jst_day(r.get("timestamp")), {
            "input": u.get("input_tokens") or 0,
            "output": u.get("output_tokens") or 0,
            "cache_read": u.get("cache_read_input_tokens") or 0,
            "cache_creation": u.get("cache_creation_input_tokens") or 0,
        }


def count_claude(projects, repo, queue_dir, totals, excluded):
    if not os.path.isdir(projects):
        return
    enc_repo = encode_cwd(repo)
    enc_queue = encode_cwd(queue_dir) + "-"
    seen = set()
    for name in sorted(os.listdir(projects)):
        if name != enc_repo and not name.startswith(enc_queue):
            continue
        pdir = os.path.join(projects, name)
        if not os.path.isdir(pdir):
            continue
        for fname in sorted(os.listdir(pdir)):
            if not fname.endswith(".jsonl"):
                continue
            sid = fname[: -len(".jsonl")]
            records = list(read_jsonl(os.path.join(pdir, fname)))
            cwd, prompt = claude_first(records)
            outside = cwd is not None and not in_scope(norm(cwd), repo, queue_dir)
            actor = None if outside else classify(prompt, norm(cwd) if cwd else None, queue_dir)
            if actor is None:
                # Count only the sessions that used tokens in the period.
                if totals.touches(r.get("timestamp") for r in records if r.get("type") == "assistant"):
                    excluded["claude_cwd" if outside else "claude_prompt"] += 1
                continue
            for day, usage in claude_usage(records, seen):
                totals.add("claude", actor, day, sid, usage)
            sub = os.path.join(pdir, sid, "subagents")
            if os.path.isdir(sub):
                for sname in sorted(os.listdir(sub)):
                    if sname.endswith(".jsonl"):
                        for day, usage in claude_usage(read_jsonl(os.path.join(sub, sname)), seen):
                            totals.add("claude", actor + "+sub", day, sid + "/" + sname, usage)


# Codex


def codex_usage(u):
    inp = u.get("input_tokens") or 0
    cached = u.get("cached_input_tokens") or 0
    return {
        "input": inp - cached,  # input includes cached
        "cache_read": cached,
        "cache_creation": u.get("cache_write_input_tokens") or 0,
        "output": u.get("output_tokens") or 0,  # includes reasoning
    }


def read_rollout(path):
    meta_id = cwd = prompt = None
    records, counts = [], []
    for r in read_jsonl(path):
        kind, p = r.get("type"), r.get("payload") or {}
        if kind == "session_meta" and meta_id is None:
            meta_id, cwd = p.get("id"), p.get("cwd")
        elif kind == "token_usage_record":
            records.append((r.get("timestamp"), p))
        elif kind == "event_msg" and p.get("type") == "token_count":
            counts.append((r.get("timestamp"), p.get("info") or {}))
        elif (
            prompt is None
            and kind == "response_item"
            and p.get("type") == "message"
            and p.get("role") == "user"
        ):
            text = text_of(p.get("content")).strip()
            if text and not text.startswith("# AGENTS.md") and not text.startswith("<"):
                prompt = text
    return {"id": meta_id, "cwd": cwd, "prompt": prompt, "records": records, "counts": counts}


def count_codex(sessions, repo, queue_dir, totals, excluded, fallbacks):
    rollouts = []
    for root, _, files in os.walk(sessions):
        for f in sorted(files):
            if f.endswith(".jsonl"):
                rollouts.append(read_rollout(os.path.join(root, f)))
    # The actor of each root thread from its own first prompt and cwd.
    actors = {}
    for r in rollouts:
        child = any(p.get("session_id") not in (None, r["id"]) for _, p in r["records"])
        if child or r["id"] is None:
            continue
        if r["cwd"] is None or not in_scope(norm(r["cwd"]), repo, queue_dir):
            actors[r["id"]] = None
            continue
        actor = classify(r["prompt"], norm(r["cwd"]), queue_dir)
        if actor is None and totals.touches(t for t, _ in r["records"] + r["counts"]):
            excluded["codex_prompt"] += 1
        actors[r["id"]] = actor
    seen = set()
    for r in rollouts:
        for timestamp, p in r["records"]:
            thread = p.get("thread_id") or r["id"]
            root = p.get("session_id") or thread
            actor = actors.get(root)
            if actor is None:
                continue
            key = (thread, p.get("response_id"))
            if key in seen:
                continue
            seen.add(key)
            totals.add("codex", actor, jst_day(timestamp), root, codex_usage(p.get("usage") or {}))
        if r["records"] or not r["counts"]:
            continue
        actor = actors.get(r["id"])
        if actor is None:
            continue
        if totals.touches(t for t, _ in r["counts"]):
            fallbacks.append(r["id"])
        previous = None
        for timestamp, info in r["counts"]:
            total = (info.get("total_token_usage") or {}).get("total_tokens")
            last = info.get("last_token_usage")
            if not last or (total is not None and total == previous):
                continue
            previous = total
            totals.add("codex", actor, jst_day(timestamp), r["id"], codex_usage(last))


# Landings


def utc_text(day, delta_days=0):
    t = dt.datetime.combine(day + dt.timedelta(days=delta_days), dt.time(), JST)
    return t.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def landings(dagq, repo, since, until, page_size):
    counts = defaultdict(int)
    after, seen = 0, set()
    while True:
        cmd = [
            dagq, "events", "--kind", "run_integrated",
            "--since", utc_text(since), "--until", utc_text(until, 1),
            "--limit", str(page_size), "--after", str(after),
        ]
        out = subprocess.run(cmd, cwd=repo, capture_output=True, text=True)
        if out.returncode != 0:
            sys.exit(f"token-usage: {' '.join(cmd)} failed: {out.stderr.strip()}")
        page = json.loads(out.stdout)
        events = page.get("events") or []
        for e in events:
            if e.get("id") in seen:
                continue
            seen.add(e.get("id"))
            day = jst_day(e.get("created_at"))
            if day:
                counts[day] += 1
        cursor = page.get("cursor")
        if len(events) < page_size or cursor is None or cursor <= after:
            break
        after = cursor
    return counts


def locate_queue_dir(dagq, repo):
    out = subprocess.run([dagq, "locate"], cwd=repo, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"token-usage: {dagq} locate failed: {out.stderr.strip()}")
    loc = json.loads(out.stdout)
    if loc.get("db"):
        return os.path.dirname(loc["db"])
    if loc.get("socket"):  # client mode: <queue dir>/service/queue.sock
        return os.path.dirname(os.path.dirname(loc["socket"]))
    sys.exit("token-usage: dagq locate gave neither db nor socket; pass --queue-dir")


def default_repo():
    out = subprocess.run(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit("token-usage: not in a Git repository; pass --repo")
    return os.path.dirname(out.stdout.strip())


# Output


def report(totals, landed, days, excluded, fallbacks, repo, queue_dir):
    rows = []
    for (day, provider, actor), r in sorted(totals.rows.items()):
        rows.append({
            "day": day, "provider": provider, "actor": actor,
            **{k: r[k] for k in FIELDS},
            "messages": r["messages"], "sessions": len(r["sessions"]),
        })
    per_landing = []
    for day in days:
        n = landed.get(day, 0)
        entry = {"day": day, "landings": n}
        for provider in ("claude", "codex"):
            sums = {k: sum(r[k] for r in rows if r["day"] == day and r["provider"] == provider) for k in FIELDS}
            entry[provider] = {k: (round(v / n) if n else None) for k, v in sums.items()}
        per_landing.append(entry)
    return {
        "repo": repo,
        "queue_dir": queue_dir,
        "queue_hash": os.path.basename(queue_dir),
        "since": days[0],
        "until": days[-1],
        "rows": rows,
        "per_landing": per_landing,
        "excluded_sessions": dict(excluded),
        "codex_fallback_rollouts": len(fallbacks),
    }


def table(header, lines):
    widths = [max(len(str(x)) for x in col) for col in zip(header, *lines)]
    fmt = lambda row: "  ".join(str(x).rjust(w) if i > 2 else str(x).ljust(w) for i, (x, w) in enumerate(zip(row, widths)))
    return "\n".join([fmt(header)] + [fmt(row) for row in lines])


def print_table(out):
    print(f"queue {out['queue_hash']} ({out['queue_dir']}), repository {out['repo']}, JST {out['since']}..{out['until']}")
    print("input is fresh input (Codex: input - cached); Codex output includes reasoning\n")
    print(table(
        ("day", "provider", "actor", *FIELDS, "messages", "sessions"),
        [(r["day"], r["provider"], r["actor"], *(r[k] for k in FIELDS), r["messages"], r["sessions"]) for r in out["rows"]],
    ))
    print("\nper landing")
    dash = lambda v: "-" if v is None else v
    print(table(
        ("day", "landings", "", *(f"claude.{k}" for k in FIELDS), *(f"codex.{k}" for k in FIELDS)),
        [(e["day"], e["landings"], "", *(dash(e["claude"][k]) for k in FIELDS), *(dash(e["codex"][k]) for k in FIELDS)) for e in out["per_landing"]],
    ))
    ex = out["excluded_sessions"]
    print(f"\nexcluded sessions (not started by dagq): claude {ex['claude_prompt']}, codex {ex['codex_prompt']}; "
          f"claude outside the repository and queue dir by cwd: {ex['claude_cwd']}")
    print(f"codex rollouts without token_usage_record (counted from token_count): {out['codex_fallback_rollouts']}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", help="repository path (default: the main checkout of the current Git repository)")
    ap.add_argument("--queue-dir", help="queue dir (default: the dir of `dagq locate` run in the repository)")
    today = dt.datetime.now(JST).date()
    ap.add_argument("--since", help="first JST day, YYYY-MM-DD (default: 6 days before --until)")
    ap.add_argument("--until", help="last JST day, inclusive, YYYY-MM-DD (default: today)")
    ap.add_argument("--format", choices=("table", "json"), default="table")
    ap.add_argument("--claude-projects", default="~/.claude/projects")
    ap.add_argument("--codex-sessions", default="~/.codex/sessions")
    ap.add_argument("--dagq", default="dagq", help="dagq executable (default: dagq on PATH)")
    ap.add_argument("--page-size", type=int, default=1000, help="events read per dagq call")
    a = ap.parse_args(argv)

    until = dt.date.fromisoformat(a.until) if a.until else today
    since = dt.date.fromisoformat(a.since) if a.since else until - dt.timedelta(days=6)
    if since > until:
        sys.exit("token-usage: --since is after --until")
    days = [(since + dt.timedelta(days=i)).isoformat() for i in range((until - since).days + 1)]
    repo = norm(a.repo) if a.repo else norm(default_repo())
    queue_dir = norm(a.queue_dir) if a.queue_dir else norm(locate_queue_dir(a.dagq, repo))

    totals = Totals(days)
    excluded = {"claude_prompt": 0, "claude_cwd": 0, "codex_prompt": 0}
    fallbacks = []
    count_claude(norm(a.claude_projects), repo, queue_dir, totals, excluded)
    count_codex(norm(a.codex_sessions), repo, queue_dir, totals, excluded, fallbacks)
    landed = landings(a.dagq, repo, since, until, a.page_size)

    out = report(totals, landed, days, excluded, fallbacks, repo, queue_dir)
    if a.format == "json":
        json.dump(out, sys.stdout, indent=2, ensure_ascii=False)
        print()
    else:
        print_table(out)


if __name__ == "__main__":
    main()
