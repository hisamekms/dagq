#!/usr/bin/env python3
"""Count the four docs metrics of goal 159 (task 1946) for a period [since, until).

Read-only: it reads main's Git history, a `dagq stats --since` JSON file and
Claude Code's transcripts, and prints JSON or Markdown tables. The definitions
and the baseline are in docs/plans/docs-slim.md; the weekly procedure is in
.claude/skills/throughput-review/reference/weekly.md.

- M1 docs/design's size and growth: for each UTC day D with D 00:00Z in the
  period, the commit `git rev-list -1 --first-parent --before=<D 00:00Z> <ref>`
  and, from `git ls-tree -r -l`, the bytes and the number of the .md files under
  docs/design and how many are over 30,720 bytes (30 KiB, the map budget of
  docs/development/documents.md). One read per day.
- M2 landings that change docs/design: the commits of `git log --first-parent
  <ref>` whose committer date is in the period are the landings; the share of
  them that change docs/design, the share that change one of the four top
  documents, and the lines added and deleted under docs/design (--numstat
  against the first parent, without rename detection).
- M3 docs conflicts and claim deferrals: from the JSON of `dagq stats --since`
  (passed as a file; the script never reads a queue), the sum of
  `conflict_hotspots.files[].conflicts` whose path is under docs/ and its share
  of the sum over every file, and `claim_deferrals`' count, seconds and
  `by_end.expired`.
- M4 tool results that are docs, from Claude Code's transcripts
  (<projects>/<dir>/<conversation>.jsonl and its subagents/*.jsonl):
  - Which: only dirs whose name ends with -runs-<run id>-worktree (a run
    worktree; not a temporary dir), and in them only conversations whose first
    user message starts with WORKER_PREFIX (worker) or REVIEW_PREFIX (review)
    and that have no model but Claude. A conversation's subagents/*.jsonl take
    its role. Everything else is counted in `skipped` only.
  - Period: a tool call counts when its tool_result's record has a UTC
    timestamp in [since, until).
  - Per call: the tool_result's characters; it points to docs/ (or src/) when
    a path of its input (Read/Edit/Write file_path, Grep path or glob, Glob path
    or pattern, any word of a Bash command), made relative to the worktree, is
    under it. Also whole Reads of a top document (no offset, no limit), Reads
    with an offset or a limit, and grep calls (Grep, or Bash running grep/rg
    not fed by a pipe).
  - Quantiles: statistics.median, and p90 by the nearest rank (the
    ceil(n * 0.9)-th of the ascending values).
  - (a) top_doc_grep: the results of a Grep whose path is a top document or a
    docs/design dir holding one, or whose glob names one, and of a Bash grep/rg
    with such a path operand; count, median and p90, worker and review.
  - (b) worker_src_per_run: for each run (the run id in the dir name; every
    worker conversation of the dir and its subagents), the characters of the
    results of a Read under src/, a Grep with a path under src/ and a Bash
    grep/rg with a path operand under src/, whatever their time. The runs are
    those whose earliest worker conversation's first record is in the period,
    a run that read no src/ counting as 0; count, median and p90.
"""

import argparse
import datetime as dt
import fnmatch
import glob
import json
import math
import os
import re
import shlex
import statistics
import subprocess
import sys
from collections import defaultdict

DESIGN = "docs/design/"
TOP_DOCS = (
    "docs/design/persistence.md",
    "docs/design/domain-model.md",
    "docs/design/supervisor-lifecycle/stats.md",
    "docs/design/provider-lifecycle.md",
)
# docs/design dirs that hold a top document (a grep there reads it).
TOP_DOC_DIRS = ("docs/design", "docs/design/supervisor-lifecycle")
MAP_LIMIT = 30720

# M4: the head of the first user message of a conversation, as the runtime
# writes it (src/application/prompt.rs: the worker's prompt and the run
# review's prompt). Any other conversation (recovery, triage, a resumed turn
# in a new transcript) is not counted.
WORKER_PREFIX = "You are executing dagq task "
REVIEW_PREFIX = "You review run "
# A project dir of a run worktree: <encoded queue dir>-runs-<run id>-worktree.
RUN_DIR = re.compile(r"-runs-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})-worktree$")
# Encoded temporary dirs (a disposable queue of a test or a smoke).
TEMP_DIRS = ("-private-var-folders-", "-var-folders-", "-tmp-", "-private-tmp-")
GREP_COMMANDS = ("grep", "rg", "egrep", "fgrep")
# Options that take a value in the next token, for grep and for rg.
GREP_VALUE = {
    "-e", "-f", "-A", "-B", "-C", "-m", "-d", "-D", "--regexp", "--file", "--after-context",
    "--before-context", "--context", "--max-count", "--include", "--exclude", "--exclude-dir",
    "--directories", "--devices", "--label",
}
RG_VALUE = {
    "-e", "-f", "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-d", "-M", "-j", "-E", "-r",
    "--regexp", "--file", "--glob", "--iglob", "--type", "--type-not", "--type-add",
    "--max-count", "--after-context", "--before-context", "--context", "--max-columns",
    "--threads", "--encoding", "--sort", "--sortr", "--max-depth", "--replace", "--pre",
    "--pre-glob", "--max-filesize", "--path-separator", "--color", "--colors",
}
SEPARATORS = {";", "|", "||", "&", "&&", "(", ")", "\n"}


def parse_time(text):
    """A UTC time from YYYY-MM-DD or an RFC 3339 time."""
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", text):
        text += "T00:00:00Z"
    value = dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
    if value.tzinfo is None:
        value = value.replace(tzinfo=dt.timezone.utc)
    return value.astimezone(dt.timezone.utc)


def iso(value):
    return value.strftime("%Y-%m-%dT%H:%M:%SZ")


def git(repo, *args):
    out = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"docs-metrics: git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


def median(values):
    return statistics.median(values) if values else None


def p90(values):
    """The nearest rank: the ceil(n * 0.9)-th of the ascending values."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * 0.9) - 1]


def ratio(part, whole):
    return round(part / whole, 4) if whole else None


# --- M1 -------------------------------------------------------------------


def m1(repo, ref, since, until):
    days = []
    day = since.replace(hour=0, minute=0, second=0, microsecond=0)
    if day < since:
        day += dt.timedelta(days=1)
    previous = None
    while day < until:
        commit = git(repo, "rev-list", "-1", "--first-parent", f"--before={iso(day)}", ref).strip()
        total = docs = over = 0
        if commit:
            for line in git(repo, "ls-tree", "-r", "-l", commit, "--", "docs/design").splitlines():
                meta, path = line.split("\t", 1)
                size = meta.split()[3]
                if path.startswith(DESIGN) and path.endswith(".md") and size != "-":
                    total += int(size)
                    docs += 1
                    over += int(size) > MAP_LIMIT
        days.append({
            "day": day.strftime("%Y-%m-%d"),
            "commit": commit or None,
            "bytes": total,
            "diff": None if previous is None else total - previous,
            "docs": docs,
            "over_30kib": over,
        })
        previous = total
        day += dt.timedelta(days=1)
    growth = days[-1]["bytes"] - days[0]["bytes"] if len(days) > 1 else None
    return {
        "days": days,
        "growth": growth,
        "growth_per_day": round(growth / (len(days) - 1)) if growth is not None else None,
    }


# --- M2 -------------------------------------------------------------------


def m2(repo, ref, since, until):
    log = git(repo, "log", "--first-parent", "--diff-merges=first-parent", "--no-renames",
              "--numstat", "--format=%x00%H %cI", ref)
    landings = design = top = added = deleted = 0
    for chunk in log.split("\0")[1:]:
        head, _, body = chunk.partition("\n")
        sha, date = head.split()
        if not since <= parse_time(date) < until:
            continue
        landings += 1
        touched_design = touched_top = False
        for line in body.splitlines():
            parts = line.split("\t")
            if len(parts) != 3 or not parts[2].startswith(DESIGN):
                continue
            touched_design = True
            touched_top |= parts[2] in TOP_DOCS
            if parts[0] != "-":
                added += int(parts[0])
                deleted += int(parts[1])
        design += touched_design
        top += touched_top
    return {
        "landings": landings,
        "design_landings": design,
        "design_share": ratio(design, landings),
        "top_doc_landings": top,
        "top_doc_share": ratio(top, landings),
        "lines_added": added,
        "lines_deleted": deleted,
        "lines_changed": added + deleted,
    }


# --- M3 -------------------------------------------------------------------


def m3(path):
    with open(path) as f:
        stats = json.load(f)
    files = (stats.get("conflict_hotspots") or {}).get("files") or []
    total = sum(file.get("conflicts") or 0 for file in files)
    docs = [file for file in files if file.get("path", "").startswith("docs/")]
    docs_conflicts = sum(file.get("conflicts") or 0 for file in docs)
    deferrals = stats.get("claim_deferrals") or {}
    expired = (deferrals.get("by_end") or {}).get("expired") or {}
    return {
        "conflicts": total,
        "docs_conflicts": docs_conflicts,
        "docs_share": ratio(docs_conflicts, total),
        "hotspot_files": len(files),
        "docs_hotspot_files": len(docs),
        "deferrals": deferrals.get("count", 0),
        "deferral_secs": deferrals.get("secs", 0),
        "deferral_hours": round(deferrals.get("secs", 0) / 3600, 1),
        "expired": expired.get("count", 0),
        "expired_secs": expired.get("secs", 0),
    }


# --- M4 -------------------------------------------------------------------


def rel(path):
    """A path relative to the worktree: an absolute path under some
    `.../worktree/` loses that head; `./` goes."""
    if not isinstance(path, str):
        return ""
    if path.startswith("/") and "/worktree/" in path:
        path = path.rsplit("/worktree/", 1)[1]
    elif path.startswith("/") and path.endswith("/worktree"):
        path = "."
    while path.startswith("./"):
        path = path[2:]
    return path.rstrip("/") or "."


def under(path, prefix):
    """Whether path is the dir prefix (without its trailing /) or under it."""
    return path == prefix.rstrip("/") or path.startswith(prefix)


def tokenize(command):
    """Shell words; a line break ends a command like `;` (a backslash-newline
    continues it)."""
    lines = command.replace("\\\n", " ").split("\n")
    command = lines[0]
    if len(lines) > 1:
        out = []
        for line in lines:
            out += tokenize(line) + [";"]
        return out
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        return list(lexer)
    except ValueError:
        return command.split()


def segments(command):
    """The simple commands of a shell command line, split at ; | & ( ), but
    not those that read a pipe (a `... | grep x` filters output, not files)."""
    out, current, piped = [], [], False
    for token in tokenize(command):
        if token in SEPARATORS or set(token) <= set(";|&()"):
            if current and not piped:
                out.append(current)
            current, piped = [], token in ("|", "|&")
        else:
            current.append(token)
    if current and not piped:
        out.append(current)
    return out


def grep_paths(segment):
    """The path operands of a grep or rg simple command, or None when it is not one."""
    words = segment
    while words and (re.fullmatch(r"\w+=.*", words[0]) or words[0] in ("command", "env", "xargs")):
        words = words[1:]
    if not words or os.path.basename(words[0]) not in GREP_COMMANDS:
        return None
    is_rg = os.path.basename(words[0]) == "rg"
    positional, skip = [], False
    # rg --files lists files and takes no pattern.
    pattern_given = is_rg and "--files" in words
    for word in words[1:]:
        if skip:
            skip = False
            continue
        if word.startswith("-") and word != "-":
            name = word.split("=", 1)[0]
            if name in ("-e", "-f", "--regexp", "--file"):
                pattern_given = True
            if "=" not in word and name in (RG_VALUE if is_rg else GREP_VALUE):
                skip = True
            continue
        positional.append(word)
    return [rel(p) for p in (positional if pattern_given else positional[1:])]


def targets(name, data):
    """The worktree-relative paths a tool call reads or writes."""
    data = data if isinstance(data, dict) else {}
    if name in ("Read", "Edit", "Write", "MultiEdit"):
        return [rel(data.get("file_path"))]
    if name == "NotebookEdit":
        return [rel(data.get("notebook_path"))]
    if name == "Grep":
        return [rel(data.get("path") or "."), rel(data.get("glob") or "")]
    if name == "Glob":
        return [rel(data.get("path") or "."), rel(data.get("pattern") or "")]
    if name == "Bash":
        return [rel(word) for word in tokenize(data.get("command") or "")]
    return []


def grep_call(name, data):
    """Whether a tool call is a Grep or a Bash command that runs grep or rg."""
    if name == "Grep":
        return True
    if name == "Bash":
        return any(grep_paths(s) is not None for s in segments(data.get("command") or ""))
    return False


def glob_points_to_top(pattern):
    """A Grep glob points to a top document when it names one: its basename,
    `**/` and its basename, or a docs/design pattern that matches it."""
    for doc in TOP_DOCS:
        base = os.path.basename(doc)
        if pattern in (base, "**/" + base, doc):
            return True
        if pattern.startswith(DESIGN) and fnmatch.fnmatchcase(doc, pattern):
            return True
    return False


def top_doc_grep(name, data):
    """(a): a Grep whose path or glob, or a Bash grep/rg whose path operand,
    is a top document or a docs/design dir that holds one."""
    hit = lambda path: path in TOP_DOCS or path in TOP_DOC_DIRS
    if name == "Grep":
        return hit(rel(data.get("path") or ".")) or glob_points_to_top(rel(data.get("glob") or ""))
    if name == "Bash":
        return any(hit(p) for s in segments(data.get("command") or "") for p in grep_paths(s) or [])
    return False


def src_read(name, data):
    """(b): a Read under src/, a Grep whose path is src/, or a Bash grep/rg
    with a path operand under src/."""
    if name == "Read":
        return under(rel(data.get("file_path")), "src/")
    if name == "Grep":
        return under(rel(data.get("path") or "."), "src/")
    if name == "Bash":
        return any(under(p, "src/") for s in segments(data.get("command") or "") for p in grep_paths(s) or [])
    return False


def result_chars(content):
    if isinstance(content, str):
        return len(content)
    if isinstance(content, list):
        return sum(len(item.get("text") or "") for item in content if isinstance(item, dict))
    return 0


def first_text(message):
    content = (message or {}).get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                return item.get("text") or ""
    return ""


def read_transcript(path):
    """The first record's time, the first user message, the models and the
    tool calls ({name, input, chars, at}) of one jsonl."""
    first_at, prompt, models, uses, calls = None, None, set(), {}, []
    with open(path, errors="replace") as f:
        for line in f:
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            at = record.get("timestamp")
            at = parse_time(at) if isinstance(at, str) else None
            if at and first_at is None:
                first_at = at
            message = record.get("message") or {}
            if record.get("type") == "user" and prompt is None:
                prompt = first_text(message)
            if record.get("type") == "assistant":
                if message.get("model") and message["model"] != "<synthetic>":
                    models.add(message["model"])
                for item in message.get("content") or []:
                    if isinstance(item, dict) and item.get("type") == "tool_use":
                        uses[item.get("id")] = (item.get("name"), item.get("input") or {})
            if record.get("type") == "user" and isinstance(message.get("content"), list):
                for item in message["content"]:
                    if isinstance(item, dict) and item.get("type") == "tool_result":
                        name, data = uses.get(item.get("tool_use_id"), (None, {}))
                        if name:
                            calls.append({"name": name, "input": data,
                                          "chars": result_chars(item.get("content")), "at": at})
    return {"first_at": first_at, "prompt": prompt or "", "models": models, "calls": calls,
            "has_records": first_at is not None}


def role_of(prompt):
    head = prompt.lstrip()
    if head.startswith(WORKER_PREFIX):
        return "worker"
    if head.startswith(REVIEW_PREFIX):
        return "review"
    return None


def claude_only(models):
    """No model but Claude (a conversation without a model record counts)."""
    return all(m.startswith("claude") for m in models)


def conversations(projects):
    """(run id, role, main transcript, [subagent transcripts]) of each worker
    and review conversation, and the numbers left out and why."""
    found, skipped = [], defaultdict(int)
    for project in sorted(os.listdir(projects)):
        full = os.path.join(projects, project)
        if not os.path.isdir(full):
            continue
        match = RUN_DIR.search(project)
        if not match or any(t in project for t in TEMP_DIRS):
            skipped["dirs"] += 1
            continue
        for path in sorted(glob.glob(os.path.join(full, "*.jsonl"))):
            main = read_transcript(path)
            role = role_of(main["prompt"])
            if role is None:
                skipped["not_worker_or_review"] += 1
                continue
            if not claude_only(main["models"]):
                skipped["not_claude"] += 1
                continue
            subs = []
            for sub in sorted(glob.glob(os.path.join(path[:-len(".jsonl")], "subagents", "*.jsonl"))):
                transcript = read_transcript(sub)
                if claude_only(transcript["models"]):
                    subs.append(transcript)
            found.append((match.group(1), role, main, subs))
    return found, dict(skipped)


def call_stats(calls):
    chars = [c["chars"] for c in calls]
    total = sum(chars)
    docs = sum(c["chars"] for c in calls if any(under(t, "docs/") for t in targets(c["name"], c["input"])))
    src = sum(c["chars"] for c in calls if any(under(t, "src/") for t in targets(c["name"], c["input"])))
    whole_top = sum(1 for c in calls if c["name"] == "Read" and rel(c["input"].get("file_path")) in TOP_DOCS
                    and c["input"].get("offset") is None and c["input"].get("limit") is None)
    ranged = sum(1 for c in calls if c["name"] == "Read"
                 and (c["input"].get("offset") is not None or c["input"].get("limit") is not None))
    return {
        "tool_results": len(calls),
        "chars": total,
        "docs_chars": docs,
        "docs_share": ratio(docs, total),
        "src_chars": src,
        "src_share": ratio(src, total),
        "whole_top_doc_reads": whole_top,
        "grep_calls": sum(1 for c in calls if grep_call(c["name"], c["input"])),
        "ranged_reads": ranged,
        "median_chars": median(chars),
        "p90_chars": p90(chars),
    }


def m4(projects, since, until):
    found, skipped = conversations(projects)
    in_period = lambda at: at is not None and since <= at < until
    by_role = defaultdict(list)
    counted = defaultdict(int)
    worker_runs = {}  # run id -> earliest first record of its worker conversations
    src_by_run = defaultdict(int)
    for run, role, main, subs in found:
        transcripts = [main, *subs]
        calls = [c for t in transcripts for c in t["calls"] if in_period(c["at"])]
        by_role[role].extend(calls)
        if calls or in_period(main["first_at"]):
            counted[role] += 1
        if role == "worker":
            if main["first_at"] and (run not in worker_runs or main["first_at"] < worker_runs[run]):
                worker_runs[run] = main["first_at"]
            src_by_run[run] += sum(c["chars"] for t in transcripts for c in t["calls"]
                                   if src_read(c["name"], c["input"]))
    every = by_role["worker"] + by_role["review"]
    top_grep = [c["chars"] for c in every if top_doc_grep(c["name"], c["input"])]
    runs = sorted(run for run, at in worker_runs.items() if in_period(at))
    per_run = [src_by_run[run] for run in runs]
    return {
        "conversations": {"worker": counted["worker"], "review": counted["review"],
                          "all": counted["worker"] + counted["review"]},
        "skipped": skipped,
        "all": call_stats(every),
        "worker": call_stats(by_role["worker"]),
        "review": call_stats(by_role["review"]),
        "top_doc_grep": {"results": len(top_grep), "median_chars": median(top_grep),
                         "p90_chars": p90(top_grep)},
        "worker_src_per_run": {"runs": len(runs), "zero_runs": sum(1 for v in per_run if v == 0),
                               "median_chars": median(per_run), "p90_chars": p90(per_run)},
    }


# --- output ---------------------------------------------------------------


def fmt(value):
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.4g}"
    return str(value)


def table(headers, rows):
    lines = ["| " + " | ".join(headers) + " |", "|" + " --- |" * len(headers)]
    lines += ["| " + " | ".join(fmt(v) for v in row) + " |" for row in rows]
    return "\n".join(lines)


def markdown(out):
    parts = [f"# docs metrics [{out['since']}, {out['until']})"]
    if out.get("m1"):
        m = out["m1"]
        parts.append("## M1 docs/design size\n\n" + table(
            ["day", "bytes", "diff", "docs", "over 30 KiB"],
            [[d["day"], d["bytes"], d["diff"], d["docs"], d["over_30kib"]] for d in m["days"]])
            + f"\n\ngrowth {fmt(m['growth'])} bytes, {fmt(m['growth_per_day'])} bytes/day")
    if out.get("m2"):
        m = out["m2"]
        parts.append("## M2 landings that change docs/design\n\n" + table(
            ["landings", "docs/design", "share", "top 4", "share", "lines +", "lines -", "lines"],
            [[m["landings"], m["design_landings"], m["design_share"], m["top_doc_landings"],
              m["top_doc_share"], m["lines_added"], m["lines_deleted"], m["lines_changed"]]]))
    if out.get("m3"):
        m = out["m3"]
        parts.append("## M3 docs conflicts and claim deferrals\n\n" + table(
            ["conflicts", "docs/", "share", "deferrals", "hours", "expired", "expired hours"],
            [[m["conflicts"], m["docs_conflicts"], m["docs_share"], m["deferrals"],
              m["deferral_hours"], m["expired"], round(m["expired_secs"] / 3600, 1)]]))
    if out.get("m4"):
        m = out["m4"]
        rows = [[role, m["conversations"].get(role), s["tool_results"], s["docs_share"], s["src_share"],
                 s["whole_top_doc_reads"], s["grep_calls"], s["ranged_reads"], s["median_chars"],
                 s["p90_chars"]] for role, s in (("all", m["all"]), ("worker", m["worker"]),
                                                 ("review", m["review"]))]
        g, r = m["top_doc_grep"], m["worker_src_per_run"]
        parts.append("## M4 tool results\n\n" + table(
            ["role", "conversations", "results", "docs share", "src share", "whole top-doc Read",
             "grep/rg", "ranged Read", "median chars", "p90 chars"], rows)
            + "\n\n(a) grep results on the top documents\n\n"
            + table(["results", "median chars", "p90 chars"],
                    [[g["results"], g["median_chars"], g["p90_chars"]]])
            + "\n\n(b) worker src/ Read and grep chars per run\n\n"
            + table(["runs", "runs with 0", "median chars", "p90 chars"],
                    [[r["runs"], r["zero_runs"], r["median_chars"], r["p90_chars"]]])
            + f"\n\nskipped: {json.dumps(m['skipped'], sort_keys=True)}")
    return "\n\n".join(parts) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--since", help="UTC start, YYYY-MM-DD or RFC 3339 (inclusive)")
    parser.add_argument("--until", help="UTC end, YYYY-MM-DD or RFC 3339 (exclusive)")
    parser.add_argument("--input", help="a JSON output saved earlier: print it as Markdown and read nothing else")
    parser.add_argument("--repo", default=".", help="the repository (M1, M2)")
    parser.add_argument("--ref", default="main", help="the branch whose history is read (M1, M2)")
    parser.add_argument("--stats", help="a file with the JSON of `dagq stats --since` (M3)")
    parser.add_argument("--claude-projects", help="Claude Code's projects dir, ~/.claude/projects (M4)")
    parser.add_argument("--metrics", default="m1,m2,m3,m4",
                        help="comma-separated metrics; M3 and M4 also need their input")
    parser.add_argument("--format", choices=("markdown", "json"), default="markdown")
    args = parser.parse_args()
    if args.input:
        with open(args.input) as f:
            sys.stdout.write(markdown(json.load(f)))
        return
    if not (args.since and args.until):
        sys.exit("docs-metrics: --since and --until are required without --input")
    since, until = parse_time(args.since), parse_time(args.until)
    if since >= until:
        sys.exit("docs-metrics: --since must be before --until")
    wanted = set(args.metrics.split(","))
    out = {"since": iso(since), "until": iso(until), "ref": args.ref,
           "m1": None, "m2": None, "m3": None, "m4": None}
    if "m1" in wanted:
        out["m1"] = m1(args.repo, args.ref, since, until)
    if "m2" in wanted:
        out["m2"] = m2(args.repo, args.ref, since, until)
    if "m3" in wanted and args.stats:
        out["m3"] = m3(args.stats)
    if "m4" in wanted and args.claude_projects:
        out["m4"] = m4(os.path.expanduser(args.claude_projects), since, until)
    if args.format == "json":
        json.dump(out, sys.stdout, indent=2, ensure_ascii=False)
        print()
    else:
        sys.stdout.write(markdown(out))


if __name__ == "__main__":
    main()
