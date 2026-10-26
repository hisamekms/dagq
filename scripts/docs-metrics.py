#!/usr/bin/env python3
"""Count the docs metrics of goals 159 and 194 (tasks 1946, 1966, 2289 and 2356) for a period [since, until).

Read-only: it reads main's Git history, a `dagq stats --since` JSON file,
Claude Code's transcripts and `dagq events --full` JSON files, and prints JSON
or Markdown tables. The definitions
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
- M5 docs-only conflicts by type, from `dagq events --full --kind ...` JSON
  files (passed as files; several pages or periods may overlap):
  - Which: the events of CONFLICT_KINDS whose payload has a non-empty
    `conflicts` list (paths), one per event id over every file, with
    `created_at` in [since, until). Docs-only: every
    path of `conflicts` is under docs/; an event with any other path is not
    counted (only `mixed`).
  - merge_base: the payload's, else `git merge-base <main> <head>`. When main
    or head is not an object of --repo or no merge base comes out, the event is
    irreproducible.
  - Per path: when it is missing at merge_base, main or head (added or
    deleted), one hunk of type 5 without merge-file; else the hunks
    (<<<<<<< to >>>>>>>) of `git merge-file -p --diff3` of main, merge_base
    and head. A path with no hunk (the rebase applies commit by commit, so the
    three ends may merge cleanly) is an irreproducible path.
  - An event is classified when a path gives a hunk, its type is the heaviest
    of its hunks' types (EVENT_ORDER), and it is partial too when some path
    gives none; it is irreproducible when no path gives a hunk.
  - A hunk's type (classify_hunk), the first that holds in HUNK_ORDER, over
    the non-blank lines of its three sides: 1 every line is a frontmatter date
    (updated, last_verified, created); 2 the merge_base side is empty and both
    others are not; 5 every line starts with `|`; 3 every line starts with
    `- `, `* ` or `<number>. ` after its indent; 4 any other hunk.
  - Shares of the types are over the classified events.
- M6 run reviews sent back for docs_drift, from `dagq events --full --kind
  review_finished` JSON files (--review-events; one per event id over every
  file, `created_at` in [since, until)):
  - The events, and their runs (distinct `run_id`, the per-run denominator).
  - Primary docs_drift: the payload's `primary_code` is docs_drift; count and
    per run.
  - Any docs_drift: some list of `reason_codes` holds docs_drift; count and
    per run.
  - Kinds of the primary docs_drift events (DRIFT_KINDS): the reason text of
    the first `reason_codes` entry holding docs_drift (the `reasons` item at
    that index; the first item of `reasons` when `reason_codes` is missing or
    the index has no text), lowercased, takes the first kind one of whose
    keywords it contains, else `other`. Shares are over the primary docs_drift
    events.
- M7 (in docs/plans/docs-reduction.md) from --run-events, and apart from it
  the program reviews' send-backs from --program-events (`m7_programs`):
  the review_programs_finished events, one per event id over every file, with
  `created_at` in [since, until). A send-back is outcome rejected, counted in
  all, per program and as docs (DOC_PROGRAMS) or other; outcome failed sends
  no worker back and is counted only by `again`; passed is not counted.
  Per day is the count over the period's days. With M7, `with_m7` adds the
  rejected ones to M7's docs_any, send-backs and reviews (a rejected review
  writes no review_finished).
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
import tempfile
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
# writes it (src/application/prompt/: the worker's prompt and the run
# review's prompt). Any other conversation (recovery, triage, a resumed turn
# in a new transcript) is not counted.
WORKER_PREFIX = "You are executing dagq task "
REVIEW_PREFIX = "You review run "
# A project dir of a run worktree: <encoded queue dir>-runs-<run id>-worktree-<run id>,
# or -worktree alone for a run whose worktree was made at runs/<run id>/worktree.
RUN_DIR = re.compile(r"-runs-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})-worktree(?:-\1)?$")
# The last component of a run worktree's path, either layout.
WORKTREE_NAME = r"worktree(?:-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})?"
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

# M5: the event kinds that record a rebase conflict with its paths
# (2026-10-07: conflict_precheck in src/application/supervise/landing.rs with
# main, head, merge_base and conflicts; landing_recheck_failed from
# src/domain/recheck.rs and integration_deferred from
# src/application/integrate.rs with main, head and conflicts, no merge_base).
CONFLICT_KINDS = ("conflict_precheck", "landing_recheck_failed", "integration_deferred")
TYPES = {1: "frontmatter_date", 2: "append", 3: "list_item", 4: "paragraph", 5: "table_or_file"}
# The order a hunk's type is tried in: the narrowest first.
HUNK_ORDER = (1, 2, 5, 3, 4)
# The order an event's type is chosen by, the heaviest (the hardest to merge
# by a rule) first.
EVENT_ORDER = (4, 5, 3, 2, 1)
# M6: the payload of review_finished (2026-10-12: judge_review in
# src/application/supervise/landing.rs records `reasons`, `reason_codes` (each
# reason's codes, in the order of `reasons`) and `primary_code` (the first code
# of the first reason, src/domain/review_reason.rs)).
REVIEW_KIND = "review_finished"
DOCS_DRIFT = "docs_drift"
# The kinds of a docs_drift reason, tried in this order; the first whose
# keyword is in the lowercased text wins. docs/plans/docs-slim.md lists the
# same keywords in the same order.
DRIFT_KINDS = (
    ("budget", ("budget", "予算", "check-design-docs", "allow list", "byte")),
    ("one_sentence_per_line", ("one sentence", "1文1行", "one line per sentence", "per line", "line length")),
    ("copy", ("copied", "copies", "copy of", "書き写", "transcri", "field list", "duplicat")),
    ("architecture_boundary", ("architecture.md",)),
    ("stale", ("stale", "still says", "still describes", "no longer", "outdated", "out of date", "古い",
               "still names", "still lists", "does not record", "doesn't record", "missing", "not updated")),
)
DRIFT_OTHER = "other"
# M7: the kinds read (2026-10-13: review_started and run_integrated from
# src/application/supervise/, review_finished as for M6 with `verdict` and,
# when review agents ran, `route` whose `destination` is send_back, ask or
# land (src/domain/review_subagents.rs)).
REVIEW_STARTED = "review_started"
RUN_INTEGRATED = "run_integrated"
SEND_BACK = "send_back"
# M7 (h): the asks that hold their run (src/domain/timeline.rs holds_the_run:
# not the observer's blocked nor the planner's question) and the worker's span
# (run_claimed to the first receipt_observed, as runs[].work of dagq stats).
ASK_OPENED, ASK_ANSWERED, ASK_CLOSED = "ask_opened", "ask_answered", "ask_closed"
ASKS_NOT_HOLDING = ("blocked", "planner_question")
RUN_CLAIMED, RECEIPT_OBSERVED = "run_claimed", "receipt_observed"
# M7's program send-backs, counted apart from M7: review_programs_finished
# (2026-10-26: review_programs_finished and review_programs_failed in
# src/application/supervise/execution/landing.rs record `outcome` passed,
# rejected (the worker is sent back with `reason`) or failed (the program
# could not run; it runs `again` or the review fails, no send-back) and
# `program`, the key of dagq.toml's [review.programs.*]).
PROGRAMS_KIND = "review_programs_finished"
# The programs that check documents (docs/, the root .md files) among
# [review.programs.*]; any other name, a retired one too, is not a document's.
# docs/plans/docs-reduction.md lists the same names.
DOC_PROGRAMS = ("check-doc-frontmatter", "check-design-docs", "check-design-sentences", "check-doc-links",
                "check-frontmatter-dates", "check-adr-numbers", "check-agents-md-size")
# A path a reason names that makes it a docs reason: one under docs/, a .md
# under plugins/, the root AGENTS.md or README.md. It starts a word: not after
# a letter, digit, `_`, `.`, `-` or `/` (so not src/docs/ nor a/README.md);
# a leading ./ is allowed. The letters are ASCII: Japanese text may touch a
# path.
DOC_PATH = re.compile(r"(?<![A-Za-z0-9_./-])(?:\./)?(?:docs/[A-Za-z0-9_./-]+"
                      r"|plugins/[A-Za-z0-9_./-]*\.md(?![A-Za-z0-9_])|AGENTS\.md(?![A-Za-z0-9_])"
                      r"|README\.md(?![A-Za-z0-9_]))")
DATE_LINE = re.compile(r"(updated|last_verified|created):")
LIST_LINE = re.compile(r"\s*([-*] |\d+\. )")


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
    `.../worktree-<run id>/` (or `.../worktree/`) loses that head; `./`
    goes."""
    if not isinstance(path, str):
        return ""
    if path.startswith("/"):
        under = re.split(r"/" + WORKTREE_NAME + r"(?:/|$)", path)
        if len(under) > 1:
            path = under[-1] or "."
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


# --- M5 -------------------------------------------------------------------


def try_git(repo, *args):
    """git's exit status and stdout as bytes, never exiting."""
    out = subprocess.run(["git", "-C", repo, *args], capture_output=True)
    return out.returncode, out.stdout


def load_events(paths):
    """The events of the files (`dagq events` objects or bare lists), one per id."""
    by_id = {}
    for path in paths:
        with open(path) as f:
            data = json.load(f)
        for event in data.get("events", []) if isinstance(data, dict) else data:
            if isinstance(event, dict) and event.get("id") is not None:
                by_id.setdefault(event["id"], event)
    return [by_id[key] for key in sorted(by_id)]


def hunks_of(merged):
    """The (main, base, head) lines of each conflict hunk of a diff3 output."""
    hunks, side = [], None
    for line in merged.splitlines():
        if line.startswith("<<<<<<<"):
            side, current = 0, ([], [], [])
        elif side is not None and line.startswith("|||||||"):
            side = 1
        elif side is not None and line.startswith("======="):
            side = 2
        elif side is not None and line.startswith(">>>>>>>"):
            hunks.append(current)
            side = None
        elif side is not None:
            current[side].append(line)
    return hunks


def classify_hunk(main, base, head):
    """The type of a hunk, the first of HUNK_ORDER that holds."""
    filled = lambda lines: [line for line in lines if line.strip()]
    lines = filled(main) + filled(base) + filled(head)
    tests = {
        1: lambda: bool(lines) and all(DATE_LINE.match(line) for line in lines),
        2: lambda: not filled(base) and bool(filled(main)) and bool(filled(head)),
        5: lambda: bool(lines) and all(line.startswith("|") for line in lines),
        3: lambda: bool(lines) and all(LIST_LINE.match(line) for line in lines),
        4: lambda: True,
    }
    return next(kind for kind in HUNK_ORDER if tests[kind]())


def path_hunks(repo, commits, path, tmp):
    """The types of a path's hunks: one 5 when it is missing at one or two
    of the commits, none when it is at none of them."""
    versions = []
    for commit in commits:
        code, out = try_git(repo, "cat-file", "blob", f"{commit}:{path}")
        versions.append(out if code == 0 else None)
    if all(v is None for v in versions):
        return []
    if any(v is None for v in versions):
        return [5]
    files = []
    for name, data in zip(("main", "base", "head"), versions):
        files.append(os.path.join(tmp, name))
        with open(files[-1], "wb") as f:
            f.write(data)
    out = subprocess.run(["git", "merge-file", "-p", "--diff3", "-L", "main", "-L", "base", "-L", "head",
                          *files], capture_output=True)
    # The exit status is the number of hunks up to 127; an error is above.
    if out.returncode > 127:
        return []
    return [classify_hunk(*h) for h in hunks_of(out.stdout.decode(errors="replace"))]


def m5(repo, paths, since, until):
    days = (until - since).total_seconds() / 86400
    conflicts, mixed = 0, 0
    by_kind = {kind: 0 for kind in CONFLICT_KINDS}
    classified = partial = irreproducible = paths_irreproducible = 0
    event_types = {kind: 0 for kind in TYPES}
    hunk_types = {kind: 0 for kind in TYPES}
    with tempfile.TemporaryDirectory(prefix="docs-metrics-m5.") as tmp:
        for event in load_events(paths):
            payload = event.get("payload") or {}
            files = payload.get("conflicts")
            at = event.get("created_at")
            if (event.get("kind") not in CONFLICT_KINDS or not isinstance(files, list) or not files
                    or not isinstance(at, str) or not since <= parse_time(at) < until):
                continue
            conflicts += 1
            if not all(isinstance(p, str) and p.startswith("docs/") for p in files):
                mixed += 1
                continue
            by_kind[event["kind"]] += 1
            main, head = payload.get("main"), payload.get("head")
            base = payload.get("merge_base")
            known = all(isinstance(c, str) and c and try_git(repo, "cat-file", "-e", f"{c}^{{commit}}")[0] == 0
                        for c in (main, head))
            if known and not (isinstance(base, str) and base):
                code, out = try_git(repo, "merge-base", main, head)
                base = out.decode().strip() if code == 0 else None
            if not known or not base:
                irreproducible += 1
                continue
            found, missing = [], 0
            for path in files:
                kinds = path_hunks(repo, (main, base, head), path, tmp)
                if kinds:
                    found += kinds
                else:
                    missing += 1
            paths_irreproducible += missing
            if not found:
                irreproducible += 1
                continue
            classified += 1
            partial += missing > 0
            event_types[next(kind for kind in EVENT_ORDER if kind in found)] += 1
            for kind in found:
                hunk_types[kind] += 1
    total = sum(by_kind.values())
    return {
        "kinds": list(CONFLICT_KINDS),
        "conflict_events": conflicts,
        "mixed_events": mixed,
        "docs_events": {**by_kind, "total": total},
        "docs_events_per_day": round(total / days, 2),
        "classified": classified,
        "partial": partial,
        "irreproducible": irreproducible,
        "types": {f"{kind}_{name}": {"events": event_types[kind], "share": ratio(event_types[kind], classified)}
                  for kind, name in TYPES.items()},
        "hunks": {"total": sum(hunk_types.values()),
                  **{f"{kind}_{name}": hunk_types[kind] for kind, name in TYPES.items()}},
        "paths_irreproducible": paths_irreproducible,
    }


# --- M6 -------------------------------------------------------------------


def drift_text(payload):
    """The reason text of the first reason whose codes hold docs_drift, or the
    first reason when the codes are missing or that index has no text."""
    reasons = payload.get("reasons")
    reasons = reasons if isinstance(reasons, list) else []
    codes = payload.get("reason_codes")
    if isinstance(codes, list):
        for index, item in enumerate(codes):
            if isinstance(item, list) and DOCS_DRIFT in item:
                if index < len(reasons) and isinstance(reasons[index], str):
                    return reasons[index]
                break
    return reasons[0] if reasons and isinstance(reasons[0], str) else ""


def drift_kind(text):
    """The first kind of DRIFT_KINDS with a keyword in the lowercased text."""
    text = text.lower()
    return next((kind for kind, words in DRIFT_KINDS if any(word in text for word in words)), DRIFT_OTHER)


def m6(paths, since, until):
    events, runs, primary, any_drift = 0, set(), 0, 0
    kinds = {kind: 0 for kind, _ in DRIFT_KINDS} | {DRIFT_OTHER: 0}
    for event in load_events(paths):
        at = event.get("created_at")
        if event.get("kind") != REVIEW_KIND or not isinstance(at, str) or not since <= parse_time(at) < until:
            continue
        payload = event.get("payload") or {}
        events += 1
        if event.get("run_id") is not None:
            runs.add(event["run_id"])
        codes = payload.get("reason_codes")
        if isinstance(codes, list) and any(isinstance(c, list) and DOCS_DRIFT in c for c in codes):
            any_drift += 1
        if payload.get("primary_code") == DOCS_DRIFT:
            primary += 1
            kinds[drift_kind(drift_text(payload))] += 1
    return {
        "review_events": events,
        "runs": len(runs),
        "primary_docs_drift": {"events": primary, "per_run": ratio(primary, len(runs))},
        "any_docs_drift": {"events": any_drift, "per_run": ratio(any_drift, len(runs))},
        "kinds": {kind: {"events": n, "share": ratio(n, primary)} for kind, n in kinds.items()},
    }


# --- M7 -------------------------------------------------------------------


def names_doc(text):
    """Whether a reason's text names a docs path (DOC_PATH)."""
    return isinstance(text, str) and DOC_PATH.search(text) is not None


def coded_docs(reasons, codes):
    """For each reason of one list, whether it is a docs reason, and the shape
    of its `reason_codes`: missing (no list), shorter or longer than
    `reasons`, and how many of its items for a reason are no list."""
    shape = {"missing": not isinstance(codes, list), "short": False, "long": False, "not_list": 0}
    if not isinstance(codes, list):
        codes = []
    else:
        shape["short"] = len(codes) < len(reasons)
        shape["long"] = len(codes) > len(reasons)
    docs = []
    for index, text in enumerate(reasons):
        item = codes[index] if index < len(codes) else None
        if index < len(codes) and not isinstance(item, list):
            shape["not_list"] += 1
        docs.append((isinstance(item, list) and DOCS_DRIFT in item) or names_doc(text))
    return docs, shape


def doc_reasons(payload):
    """For each reason a review_finished sends back, whether it is a docs
    reason, and the shape of the review's own `reason_codes` when its reasons
    are read. The reasons are put together as src/domain/mod.rs
    ReviewVerdict::sent_back_findings does: without a `route`, the review's
    `reasons`; with one, the review's `reasons` only when `route.parent` is
    `route.destination`, then, unless the route lands, the `reasons` (the
    `summary` when they are empty) of each agent of `agents` whose
    `route.agents` entry goes where the route goes, read by its own
    `reason_codes`. The review's empty `reasons` are not filled by its
    `summary`."""
    route = payload.get("route")
    reasons = payload.get("reasons")
    reasons = reasons if isinstance(reasons, list) else []
    docs, shape = [], {"missing": False, "short": False, "long": False, "not_list": 0}
    if not isinstance(route, dict) or route.get("parent") == route.get("destination"):
        docs, shape = coded_docs(reasons, payload.get("reason_codes"))
    if not isinstance(route, dict) or route.get("destination") == "land":
        return docs, shape
    deciding = {a.get("agent") for a in route.get("agents") or []
                if isinstance(a, dict) and a.get("destination") == route.get("destination")}
    for result in payload.get("agents") or []:
        if not isinstance(result, dict) or result.get("agent") not in deciding:
            continue
        texts = result.get("reasons")
        texts = texts if isinstance(texts, list) and texts else [result.get("summary")]
        docs += coded_docs(texts, result.get("reason_codes"))[0]
    return docs, shape


def send_back(payload):
    """`send_back` by the route, `revise` by the verdict without one, or None."""
    route = payload.get("route")
    if isinstance(route, dict):
        return "route" if route.get("destination") == SEND_BACK else None
    return "verdict" if payload.get("verdict") == "revise" else None


def waits(values, cut):
    return {"count": len(values), "cut": cut, "sum_secs": round(sum(values), 3),
            "sum_hours": round(sum(values) / 3600, 1), "median_secs": median(values)}


def timed(events):
    """The events with a `created_at`, by time and id."""
    return sorted((e for e in events if isinstance(e.get("created_at"), str)),
                  key=lambda e: (parse_time(e["created_at"]), e["id"]))


def ask_spans(events):
    """M7 (h): each run's union of the spans of the asks that hold it, from an
    ask_opened to the earlier of its first ask_answered and first ask_closed
    (None, past any end, when neither is in the input), and the counts."""
    opened, ends = {}, {}
    for event in timed(events):
        payload = event.get("payload") or {}
        ask = payload.get("ask_id")
        if ask is None:
            continue
        if event.get("kind") == ASK_OPENED:
            opened.setdefault(ask, event)
        elif event.get("kind") in (ASK_ANSWERED, ASK_CLOSED):
            ends.setdefault(ask, event)
    counts = {"asks": 0, "runtime_closed": 0, "unanswered": 0,
              "ends_without_opened": sum(ask not in opened for ask in ends)}
    spans = defaultdict(list)
    for ask, event in opened.items():
        if event.get("run_id") is None or (event.get("payload") or {}).get("kind") in ASKS_NOT_HOLDING:
            continue
        counts["asks"] += 1
        end = ends.get(ask)
        if end is not None and (end.get("payload") or {}).get("runtime_closed") is True:
            counts["runtime_closed"] += 1
            continue
        counts["unanswered"] += end is None
        spans[event["run_id"]].append((parse_time(event["created_at"]),
                                       parse_time(end["created_at"]) if end is not None else None))
    union = {}
    for run, items in spans.items():
        merged = []
        for start, end in sorted(items, key=lambda s: (s[0], s[1] is None, s[1])):
            if merged and (merged[-1][1] is None or start <= merged[-1][1]):
                last = merged[-1][1]
                merged[-1] = (merged[-1][0], None if last is None or end is None else max(last, end))
            else:
                merged.append((start, end))
        union[run] = merged
    return union, counts


def overlap(spans, start, end):
    """The seconds of [start, end) that the spans cover."""
    secs = 0.0
    for a, b in spans:
        low, high = max(a, start), end if b is None else min(b, end)
        if high > low:
            secs += (high - low).total_seconds()
    return secs


def worker_spans(events):
    """Each run's first run_claimed and first receipt_observed."""
    first = {}
    for event in timed(events):
        if event.get("kind") in (RUN_CLAIMED, RECEIPT_OBSERVED) and event.get("run_id") is not None:
            first.setdefault((event["run_id"], event["kind"]), parse_time(event["created_at"]))
    return first


def m7(paths, stats_path, since, until, ask_paths=()):
    days = (until - since).total_seconds() / 86400
    reviews, started, landings = [], defaultdict(list), 0
    for event in load_events(paths):
        at = event.get("created_at")
        if not isinstance(at, str) or not since <= parse_time(at) < until:
            continue
        key = (parse_time(at), event["id"])
        if event.get("kind") == REVIEW_KIND:
            reviews.append((key, event))
        elif event.get("kind") == REVIEW_STARTED and event.get("run_id") is not None:
            started[event.get("run_id")].append(key)
        elif event.get("kind") == RUN_INTEGRATED:
            landings += 1
    runs = {event.get("run_id") for _, event in reviews if event.get("run_id") is not None}
    counts = {"all": 0, "docs_any": 0, "docs_only": 0}
    by = {"route": 0, "verdict": 0}
    concern, no_reasons = 0, 0
    shapes = {"codes_missing": 0, "codes_short": 0, "codes_long": 0, "code_items_not_list": 0}
    durations = {name: [] for name in counts}
    spans = {name: [] for name in counts}
    cut = {name: 0 for name in counts}
    for key, event in reviews:
        payload = event.get("payload") or {}
        how = send_back(payload)
        if how is None:
            concern += payload.get("verdict") == "concern" and not isinstance(payload.get("route"), dict)
            continue
        by[how] += 1
        docs, shape = doc_reasons(payload)
        shapes["codes_missing"] += shape["missing"]
        shapes["codes_short"] += shape["short"]
        shapes["codes_long"] += shape["long"]
        shapes["code_items_not_list"] += shape["not_list"]
        no_reasons += not docs
        groups = ["all"] + (["docs_any"] if any(docs) else []) + (["docs_only"] if docs and all(docs) else [])
        run = event.get("run_id")
        later = [k for k in started.get(run, []) if k > key] if run is not None else []
        for name in groups:
            counts[name] += 1
            if later:
                durations[name].append((min(later)[0] - key[0]).total_seconds())
                spans[name].append((run, key[0], min(later)[0]))
            else:
                cut[name] += 1
    work = None
    if stats_path:
        with open(stats_path) as f:
            stats = json.load(f)
        values = [r["work"] for r in stats.get("runs") or [] if isinstance(r, dict)
                  and isinstance(r.get("work"), (int, float))]
        work = {"field": "runs[].work", "runs": len(values), "median_secs": median(values)}
    ask_waits = waits_without_asks = worker_work_without_asks = None
    if ask_paths:
        asks = load_events(ask_paths)
        held, ask_waits = ask_spans(asks)
        waits_without_asks = {}
        for name, items in spans.items():
            left, taken = [], 0.0
            for run, start, end in items:
                secs = overlap(held.get(run, []), start, end)
                taken += secs
                left.append((end - start).total_seconds() - secs)
            waits_without_asks[name] = {**waits(left, cut[name]), "subtracted_secs": round(taken, 3)}
        if stats_path:
            first = worker_spans(asks)
            left, taken, mismatched, absent = [], 0.0, 0, 0
            for r in stats.get("runs") or []:
                if not (isinstance(r, dict) and isinstance(r.get("work"), (int, float))):
                    continue
                start, end = first.get((r.get("run_id"), RUN_CLAIMED)), first.get((r.get("run_id"), RECEIPT_OBSERVED))
                if start is None or end is None:
                    absent += 1
                    continue
                if abs((end - start).total_seconds() - r["work"]) > 1:
                    mismatched += 1
                    continue
                secs = overlap(held.get(r["run_id"], []), start, end)
                taken += secs
                left.append(round(r["work"] - secs, 3))
            worker_work_without_asks = {"runs": len(left), "median_secs": median(left),
                                        "subtracted_secs": round(taken, 3), "mismatched": mismatched,
                                        "absent": absent}
    review_count = len(reviews)
    wait = {name: waits(durations[name], cut[name]) for name in counts}
    per_day = lambda n: round(n / days, 2)
    return {
        "days": round(days, 2),
        "reviews": {"events": review_count, "runs": len(runs)},
        "send_backs": {**counts, "by_route": by["route"], "by_verdict": by["verdict"],
                       "without_reasons": no_reasons},
        "concern_without_route": concern,
        "reason_codes": shapes,
        "waits": wait,
        "landings": landings,
        "worker_work": work,
        "ask_waits": ask_waits,
        "waits_without_asks": waits_without_asks,
        "worker_work_without_asks": worker_work_without_asks,
        "per_review": {"send_backs": ratio(counts["all"], review_count),
                       "docs_any": ratio(counts["docs_any"], review_count),
                       "docs_only": ratio(counts["docs_only"], review_count)},
        "per_landing": {"reviews": ratio(review_count, landings),
                        "wait_hours": ratio(wait["all"]["sum_secs"] / 3600, landings),
                        "docs_any_wait_hours": ratio(wait["docs_any"]["sum_secs"] / 3600, landings),
                        "docs_only_wait_hours": ratio(wait["docs_only"]["sum_secs"] / 3600, landings)},
        "per_day": {"reviews": per_day(review_count), "send_backs": per_day(counts["all"]),
                    "docs_any": per_day(counts["docs_any"]), "docs_only": per_day(counts["docs_only"]),
                    "wait_hours": per_day(wait["all"]["sum_secs"] / 3600), "landings": per_day(landings)},
    }


def m7_programs(paths, since, until, m7_out=None):
    days = (until - since).total_seconds() / 86400
    rejected, by_program, failed = {"all": 0, "docs": 0, "other": 0}, defaultdict(int), {"again": 0, "not_again": 0}
    for event in load_events(paths):
        at = event.get("created_at")
        if event.get("kind") != PROGRAMS_KIND or not isinstance(at, str) or not since <= parse_time(at) < until:
            continue
        payload = event.get("payload") or {}
        if payload.get("outcome") == "rejected":
            program = payload.get("program")
            rejected["all"] += 1
            rejected["docs" if program in DOC_PROGRAMS else "other"] += 1
            by_program[str(program)] += 1
        elif payload.get("outcome") == "failed":
            failed["again" if payload.get("again") is True else "not_again"] += 1
    combined = None
    if m7_out:
        docs = m7_out["send_backs"]["docs_any"] + rejected["docs"]
        combined = {"docs_share": ratio(docs, m7_out["send_backs"]["all"] + rejected["all"]),
                    "docs_per_review": ratio(docs, m7_out["reviews"]["events"] + rejected["all"])}
    return {
        "days": round(days, 2),
        "send_backs": rejected,
        "per_day": {"send_backs": round(rejected["all"] / days, 2), "docs": round(rejected["docs"] / days, 2)},
        "by_program": dict(sorted(by_program.items())),
        "failed": failed,
        "with_m7": combined,
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
    if out.get("m5"):
        m = out["m5"]
        e = m["docs_events"]
        parts.append("## M5 docs-only conflicts\n\n" + table(
            ["conflict events", "mixed", *m["kinds"], "docs-only", "per day", "classified", "partial",
             "irreproducible", "irreproducible paths"],
            [[m["conflict_events"], m["mixed_events"], *[e[k] for k in m["kinds"]], e["total"],
              m["docs_events_per_day"], m["classified"], m["partial"], m["irreproducible"],
              m["paths_irreproducible"]]])
            + "\n\nTypes (shares over the classified events)\n\n"
            + table(["type", "events", "share", "hunks"],
                    [[name, t["events"], t["share"], m["hunks"][name]] for name, t in m["types"].items()]
                    + [["total", m["classified"], None, m["hunks"]["total"]]]))
    if out.get("m6"):
        m = out["m6"]
        p, a = m["primary_docs_drift"], m["any_docs_drift"]
        parts.append("## M6 run reviews sent back for docs_drift\n\n" + table(
            ["review_finished", "runs", "primary docs_drift", "per run", "any docs_drift", "per run"],
            [[m["review_events"], m["runs"], p["events"], p["per_run"], a["events"], a["per_run"]]])
            + "\n\nKinds of the primary docs_drift events (shares over them)\n\n"
            + table(["kind", "events", "share"],
                    [[name, k["events"], k["share"]] for name, k in m["kinds"].items()]
                    + [["total", p["events"], None]]))
    if out.get("m7"):
        m = out["m7"]
        s, w, c = m["send_backs"], m["waits"], m["reason_codes"]
        work = m["worker_work"] or {}
        parts.append("## M7 run reviews, send-backs and their waits\n\n" + table(
            ["days", "reviews", "runs", "send-backs", "with docs", "docs only", "by route", "by verdict",
             "without reasons", "concern without route", "landings", "worker work runs", "work median secs"],
            [[m["days"], m["reviews"]["events"], m["reviews"]["runs"], s["all"], s["docs_any"], s["docs_only"],
              s["by_route"], s["by_verdict"], s["without_reasons"], m["concern_without_route"], m["landings"],
              work.get("runs"), work.get("median_secs")]])
            + "\n\nWaits from a send-back to its run's next review_started (cut ones left out)\n\n"
            + table(["send-backs", "waits", "cut", "hours", "median secs", "hours per landing"],
                    [[name, w[name]["count"], w[name]["cut"], w[name]["sum_hours"], w[name]["median_secs"],
                      m["per_landing"][key]]
                     for name, key in (("all", "wait_hours"), ("docs_any", "docs_any_wait_hours"),
                                       ("docs_only", "docs_only_wait_hours"))])
            + "\n\nPer review, per landing and per day\n\n"
            + table(["send-backs per review", "with docs per review", "docs only per review",
                     "reviews per landing", "reviews per day", "send-backs per day", "with docs per day",
                     "docs only per day",
                     "wait hours per day", "landings per day"],
                    [[m["per_review"]["send_backs"], m["per_review"]["docs_any"], m["per_review"]["docs_only"],
                      m["per_landing"]["reviews"], m["per_day"]["reviews"], m["per_day"]["send_backs"],
                      m["per_day"]["docs_any"], m["per_day"]["docs_only"], m["per_day"]["wait_hours"],
                      m["per_day"]["landings"]]])
            + "\n\nreason_codes of the send-backs\n\n"
            + table(["missing", "shorter than reasons", "longer than reasons", "items not a list"],
                    [[c["codes_missing"], c["codes_short"], c["codes_long"], c["code_items_not_list"]]]))
    if out.get("m7_programs"):
        m = out["m7_programs"]
        s, f, w = m["send_backs"], m["failed"], m["with_m7"] or {}
        parts.append("## M7 program send-backs (apart from M7)\n\n" + table(
            ["days", "rejected", "docs", "other", "per day", "docs per day", "failed again", "failed not again",
             "docs share with M7", "docs per review with M7"],
            [[m["days"], s["all"], s["docs"], s["other"], m["per_day"]["send_backs"], m["per_day"]["docs"],
              f["again"], f["not_again"], w.get("docs_share"), w.get("docs_per_review")]])
            + "\n\nRejected per program\n\n"
            + table(["program", "rejected"], [[name, n] for name, n in m["by_program"].items()]))
    return "\n\n".join(parts) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--since", help="UTC start, YYYY-MM-DD or RFC 3339 (inclusive)")
    parser.add_argument("--until", help="UTC end, YYYY-MM-DD or RFC 3339 (exclusive)")
    parser.add_argument("--input", help="a JSON output saved earlier: print it as Markdown and read nothing else")
    parser.add_argument("--repo", default=".", help="the repository (M1, M2, M5)")
    parser.add_argument("--ref", default="main", help="the branch whose history is read (M1, M2)")
    parser.add_argument("--stats", help="a file with the JSON of `dagq stats --since` (M3)")
    parser.add_argument("--claude-projects", help="Claude Code's projects dir, ~/.claude/projects (M4)")
    parser.add_argument("--events", nargs="+", action="extend", default=[],
                        help="files with the JSON of `dagq events --full --kind ...` (M5); "
                             "an event id in several files counts once")
    parser.add_argument("--review-events", nargs="+", action="extend", default=[],
                        help="files with the JSON of `dagq events --full --kind review_finished` (M6); "
                             "an event id in several files counts once")
    parser.add_argument("--run-events", nargs="+", action="extend", default=[],
                        help="files with the JSON of `dagq events --full --kind review_finished --kind "
                             "review_started --kind run_integrated` (M7, with --stats for the worker time); "
                             "an event id in several files counts once")
    parser.add_argument("--ask-events", nargs="+", action="extend", default=[],
                        help="files with the JSON of `dagq events --full --kind ask_opened --kind ask_answered "
                             "--kind ask_closed --kind run_claimed --kind receipt_observed` (M7 (h), not "
                             "narrowed to the period); an event id in several files counts once")
    parser.add_argument("--program-events", nargs="+", action="extend", default=[],
                        help="files with the JSON of `dagq events --full --kind review_programs_finished` "
                             "(M7's program send-backs, counted apart from M7); an event id in several files "
                             "counts once")
    parser.add_argument("--metrics", default="m1,m2,m3,m4,m5,m6,m7",
                        help="comma-separated metrics; M3, M4, M5, M6 and M7 also need their input")
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
           "m1": None, "m2": None, "m3": None, "m4": None, "m5": None, "m6": None, "m7": None,
           "m7_programs": None}
    if "m1" in wanted:
        out["m1"] = m1(args.repo, args.ref, since, until)
    if "m2" in wanted:
        out["m2"] = m2(args.repo, args.ref, since, until)
    if "m3" in wanted and args.stats:
        out["m3"] = m3(args.stats)
    if "m4" in wanted and args.claude_projects:
        out["m4"] = m4(os.path.expanduser(args.claude_projects), since, until)
    if "m5" in wanted and args.events:
        out["m5"] = m5(args.repo, args.events, since, until)
    if "m6" in wanted and args.review_events:
        out["m6"] = m6(args.review_events, since, until)
    if "m7" in wanted and args.run_events:
        out["m7"] = m7(args.run_events, args.stats, since, until, args.ask_events)
    if "m7" in wanted and args.program_events:
        out["m7_programs"] = m7_programs(args.program_events, since, until, out["m7"])
    if args.format == "json":
        json.dump(out, sys.stdout, indent=2, ensure_ascii=False)
        print()
    else:
        sys.stdout.write(markdown(out))


if __name__ == "__main__":
    main()
