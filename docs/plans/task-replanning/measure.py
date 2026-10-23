#!/usr/bin/env python3
"""Baseline of long non-interactive runs for the task replanning plan
(docs/plans/task-replanning.md, section 長期化の測定).

Reads only `dagq events` and `dagq stats` (read-only commands); never opens
the queue DB and changes nothing.

  python3 -I measure.py --claims-from 2026-10-01 --claims-to 2026-10-04 \
      --events-to 2026-10-06 [--out DIR]

Population: every run whose `run_claimed` has worker_mode headless and was
recorded in [claims-from, claims-to); the run-level tables count these only.
The task level selects the tasks of those runs and adds every run of them
claimed before events-to (re-runs in the grace period and runs of another
mode included, marked population=False in runs.csv). Events are read up to
events-to (exclusive); a run or task with no end by then is reported as
open, not as zero. The same command after the replanning lands, with a
window of the same length and the same change groups, gives the after side.

A task replaced by a replan names its children with `--replaced SRC:C1,C2`
(read from `dagq replans --all`; give only the children the condition table
needs), once per replacement, in any order. A child replaced again is given
its own `--replaced`, so the replacements form a tree whose leaves are the
tasks not replaced. A tree counts only when its root is a selected task;
then every task of the tree (the intermediate ones too) keeps its runs and
sessions, claimed up to events-to, and all of them are added to the row of
the root. A tree whose root ran only before the claim window is one row
excluded like a task with earlier runs. A replaced task ends when every leaf
under it has ended: its time is the latest leaf end, and its end is
`replaced` when every leaf completed and `replaced_child_canceled` when a
leaf was canceled. Until then it is open. An empty child list or a leaf with
no run stays open, and a cycle is reported as `replacement_cycle` (never as
an end).

Per run it counts turn_started / turn_finished, the review_finished
verdicts, and classifies each turn_started by cause:
  initial          the task's prompt
  review           revise request, or a resume after the review's findings
                   were sent back (landing_decided send_back)
  conflict         conflict request, or a resume after a rebase conflict
                   (conflict_precheck, landing_recheck_failed, resume reason)
  e2e              a resume after run_e2e_failed
  verify           a resume after a verification command failed on rebase
  receipt          a resume after validation_finished accepted=false
  worker_question  answer of an ask
  provider_host    provider switch, or the turn after a failed turn_finished
                   (heartbeat loss, usage limit, ...)
  unlabeled        none of these (kept apart, never merged into the others)
A resolution request takes the cause of the latest trigger before it.

Writes DIR/runs.csv (one row per run) and DIR/tasks.csv (one row per task:
all its runs plus every session_closed carrying its task ID from the first
claim to the task's end, split into worker / review_job / recovery_job /
planning / other with the sessions missing tokens or cost counted apart),
and prints the summary the plan quotes. `--task-windows 1405 --cutoff N ...` counts #1405's turns and
review verdicts per run up to each cutoff; its per-cause split, cost and
waits come from the event IDs the plan lists.
"""

import argparse
import collections
import csv
import datetime
import json
import os
import subprocess
import sys

KINDS = [
    "run_claimed", "turn_started", "turn_finished", "review_finished",
    "resume_started", "conflict_precheck", "landing_recheck_failed",
    "run_e2e_failed", "run_e2e_finished", "ask_opened", "ask_answered",
    "validation_finished", "run_integrated", "run_inherited",
    "recovery_requested", "task_status_changed", "landing_decided",
]
WAIT_ASKS = {"approve_landing", "decide", "worker_question", "stalled", "blocked"}


def dagq(*args):
    out = subprocess.run(["dagq", *args], check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def ts(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def pct(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))]


def resume_cause(reason):
    if "review's findings were sent back" in reason:
        return "review"
    if "e2e failed" in reason:
        return "e2e"
    if "conflict" in reason:
        return "conflict"
    if "heartbeat" in reason or "environment failure" in reason or "stopped from outside" in reason:
        return "provider_host"
    if "verification command" in reason or "exited with" in reason or "exits with" in reason:
        return "verify"
    return "unlabeled"


def classify(events, claims_from, claims_to, replaced=None):
    """Every run of the population (headless, claimed in [claims_from,
    claims_to)) and every later or other-mode run of the same tasks claimed
    before the end of the events, so a task's re-runs during the grace period
    are counted with it. `population` marks the runs of the run-level
    tables; the others only add to their task. The tasks of a replacement
    tree (`replaced`) are added only when the tree's root is a selected task,
    so a tree that starts outside the claim window adds no row."""
    claims = [e for e in events if e["kind"] == "run_claimed" and e["created_at"] >= claims_from]
    population_tasks = {
        e["task_id"] for e in claims
        if (e.get("payload") or {}).get("worker_mode") == "headless" and e["created_at"] < claims_to}
    roots = replacement_roots(replaced or {}, population_tasks)
    population_tasks |= {task for task, root in roots.items() if root in population_tasks}
    runs = {}
    for e in claims:
        p = e.get("payload") or {}
        if e["task_id"] in population_tasks:
            runs[e["run_id"]] = dict(
                population=p.get("worker_mode") == "headless" and e["created_at"] < claims_to,
                worker_mode=p.get("worker_mode"),
                task=e["task_id"], claim=e["created_at"], provider=p.get("provider"),
                started=0, finished=0, turns=collections.Counter(),
                verdicts=collections.Counter(), reviews=[], trig=None, failed_prev=False,
                fail_turns=0, asks=[], cost=0.0, cost_missing=0, out=0, tok_missing=0,
                end=None, inherited=False, receipt_rejected=0)
    answered = {}
    for e in events:
        if e["kind"] == "ask_answered":
            answered[e["payload"].get("ask_id")] = e["created_at"]
    for e in events:
        r = runs.get(e.get("run_id"))
        if r is None:
            continue
        k, p = e["kind"], e.get("payload") or {}
        if k == "run_inherited":
            r["inherited"] = True
        elif k == "review_finished":
            r["verdicts"][p.get("verdict")] += 1
            r["reviews"].append((e["id"], p.get("verdict"), p.get("primary_code")))
            if p.get("verdict") != "pass":
                r["trig"] = "review"
        elif k == "landing_decided" and p.get("answer") == "send_back":
            r["trig"] = "review"
        elif k in ("conflict_precheck",) or (k == "landing_recheck_failed" and p.get("code") == "rebase_conflict"):
            r["trig"] = "conflict"
        elif k == "landing_recheck_failed":
            r["trig"] = "verify"
        elif k == "run_e2e_failed":
            r["trig"] = "e2e"
        elif k == "validation_finished" and p.get("accepted") is False:
            r["receipt_rejected"] += 1
            r["trig"] = "receipt"
        elif k == "resume_started":
            cause = resume_cause(p.get("reason") or "")
            if cause != "unlabeled" or r["trig"] is None:
                r["trig"] = cause
        elif k == "ask_opened":
            r["asks"].append((p.get("ask_id"), p.get("kind"), e["created_at"]))
        elif k == "turn_started":
            r["started"] += 1
            what = p.get("what") or ""
            if what == "the task's prompt":
                cause = "initial"
            elif what == "revise request":
                cause = "review"
            elif what == "conflict request":
                cause = "conflict"
            elif what.startswith("answer of ask"):
                cause = "worker_question"
            elif what == "provider switch":
                cause = "provider_host"
            elif what == "resolution request":
                cause = r["trig"] or "unlabeled"
            else:
                cause = "unlabeled"
            if r["failed_prev"]:
                cause = "provider_host"
            r["failed_prev"] = False
            r["turns"][cause] += 1
        elif k == "turn_finished":
            r["finished"] += 1
            if p.get("failure"):
                r["fail_turns"] += 1
                r["failed_prev"] = True
            if p.get("cost_usd") is None:
                r["cost_missing"] += 1
            else:
                r["cost"] += p["cost_usd"]
            tok = p.get("tokens")
            if tok:
                r["out"] += tok.get("output") or 0
            else:
                r["tok_missing"] += 1
        elif k == "run_integrated":
            r["end"] = ("integrated", e["created_at"])
    for e in events:
        if e["kind"] == "task_status_changed" and e["payload"].get("from") == "in_progress":
            for r in runs.values():
                if r["task"] == e["task_id"] and r["end"] is None and e["created_at"] > r["claim"]:
                    r["end"] = (e["payload"].get("to"), e["created_at"])
    for r in runs.values():
        run_any = best_any = 0
        detect = None
        for eid, verdict, _ in r["reviews"]:
            if verdict == "pass":
                run_any = 0
                continue
            run_any += 1
            best_any = max(best_any, run_any)
            if run_any == 3 and detect is None:
                detect = eid
        r["best_nonpass_streak"] = best_any
        r["detect"] = detect
        r["nonpass_after_detect"] = (
            sum(1 for eid, v, _ in r["reviews"] if detect and eid > detect and v != "pass"))
        r["turns_after_detect"] = None
        wait = 0.0
        wait_missing = 0
        for ask_id, kind, at in r["asks"]:
            if kind in WAIT_ASKS:
                if ask_id in answered:
                    wait += (ts(answered[ask_id]) - ts(at)).total_seconds()
                else:
                    wait_missing += 1
        r["human_wait_secs"] = wait
        r["human_wait_missing"] = wait_missing
    for e in events:
        r = runs.get(e.get("run_id"))
        if r and r["detect"] and e["kind"] == "turn_started" and e["id"] > r["detect"]:
            r["turns_after_detect"] = (r["turns_after_detect"] or 0) + 1
    for r in runs.values():
        if r["detect"] and r["turns_after_detect"] is None:
            r["turns_after_detect"] = 0
    return runs


# Session kinds of session_closed, by the component of the task-level cost.
COMPONENTS = {
    "worker": "worker", "revise": "worker", "resume": "worker",
    "review": "review_job", "triage": "recovery_job",
    "runtime_planner": "planning", "plan_review": "planning", "planner": "planning",
}


def parse_replaced(specs):
    """{source: [children]} from the `--replaced SRC:C1,C2` arguments; a
    source given twice is refused (its children belong in one argument)."""
    replaced = {}
    for spec in specs:
        src, kids = spec.split(":")
        if int(src) in replaced:
            raise ValueError(f"task {src} is given twice in --replaced")
        replaced[int(src)] = [int(k) for k in kids.split(",") if k]
    return replaced


CYCLE = "replacement_cycle"
DONE = ("completed", "canceled", "replaced", "replaced_child_canceled")
# The ends that met the original conditions (every leaf completed).
ACHIEVED = ("completed", "replaced")


def replacement_ends(own_ends, replaced):
    """The end of every task named in `replaced` ({source: [children]}),
    worked out from the leaves under it, whatever order the replacements
    came in. own_ends holds each task's own (status, time) end, if any; a
    replaced task's own status never ends it. A result is (label, time), None
    while a leaf is unfinished, a child list is empty or a leaf has no run,
    and (CYCLE, None) when the replacements loop."""
    memo = {}

    def end_of(task, stack):
        if task in stack:
            return (CYCLE, None)
        if task in memo:
            return memo[task]
        if task not in replaced:
            result = own_ends.get(task)
        else:
            kids = replaced[task]
            got = [end_of(k, stack | {task}) for k in kids]
            if any(g and g[0] == CYCLE for g in got):
                result = (CYCLE, None)
            elif not kids or any(g is None for g in got):
                result = None
            else:
                canceled = any(g[0] in ("canceled", "replaced_child_canceled") for g in got)
                result = ("replaced_child_canceled" if canceled else "replaced",
                          max(g[1] for g in got))
        memo[task] = result
        return result

    return {task: end_of(task, frozenset()) for task in replaced}


def replacement_parents(replaced):
    """{child: source}; a task named as the child of two sources is
    refused."""
    parent = {}
    for src in sorted(replaced):
        for kid in replaced[src]:
            if kid in parent and parent[kid] != src:
                raise ValueError(f"task {kid} is a child of both {parent[kid]} and {src}")
            parent[kid] = src
    return parent


def replacement_roots(replaced, preferred=()):
    """The root of every task in the replacement tree ({task: root}). In a
    loop the root is the smallest task ID on it among `preferred` (the
    selected tasks), or the smallest on it when none is."""
    parent = replacement_parents(replaced)
    roots = {}
    for task in set(parent) | set(replaced):
        walk = [task]
        while walk[-1] in parent and parent[walk[-1]] not in walk:
            walk.append(parent[walk[-1]])
        top = walk[-1]
        if top in parent:
            loop = walk[walk.index(parent[top]):]
            top = min([t for t in loop if t in preferred] or loop)
        roots[task] = top
    return roots


def task_level(runs, events, sessions, earlier_claims, events_to, replaced=None):
    """One row per task of the population: every run of the task claimed
    before the end of the events (re-runs after the claim window included)
    and every session_closed carrying the task's ID from its first claim to
    its end, with a replaced task's children folded into it, so
    re-runs, review jobs, recovery jobs and planning during the task count
    the same way before and after the replanning lands."""
    tasks = {}
    for rid, r in runs.items():
        t = tasks.setdefault(r["task"], dict(task=r["task"], runs=[], first_claim=r["claim"]))
        t["runs"].append(r)
        t["first_claim"] = min(t["first_claim"], r["claim"])
    for e in events:
        t = tasks.get(e.get("task_id"))
        if (t and e["kind"] == "task_status_changed" and "end" not in t
                and e["payload"].get("to") in ("completed", "canceled")
                and e["created_at"] > t["first_claim"]):
            t["end"] = (e["payload"]["to"], e["created_at"])
    replaced = replaced or {}
    own_ends = {task: t["end"] for task, t in tasks.items() if "end" in t}
    for task, result in replacement_ends(own_ends, replaced).items():
        t = tasks.get(task)
        if not t:
            continue
        t.pop("end", None)
        if result and result[0] == CYCLE:
            t["unresolved"] = CYCLE
        elif result:
            t["end"] = result
    for t in tasks.values():
        t["excluded"] = t["task"] in earlier_claims
        t["comp"] = collections.defaultdict(collections.Counter)
    for e in sessions:
        t = tasks.get(e.get("task_id"))
        if not t:
            continue
        end_at = t["end"][1] if "end" in t else events_to
        if not (t["first_claim"] <= e["created_at"] <= end_at):
            continue
        p = e["payload"]
        c = t["comp"][COMPONENTS.get(p.get("kind"), "other")]
        tok = p.get("tokens") or {}
        c["sessions"] += 1
        if tok.get("output") is None:
            c["tokens_missing"] += 1
        else:
            c["output_tokens"] += tok["output"]
        if tok.get("cost_usd") is None:
            c["cost_missing"] += 1
        else:
            c["cost_usd_milli"] += int(round(tok["cost_usd"] * 1000))
    selected = {task for task, t in tasks.items() if any(r["population"] for r in t["runs"])}
    parent = replacement_parents(replaced)
    trees = collections.defaultdict(list)
    for task, root in replacement_roots(replaced, selected).items():
        trees[root].append(task)
    for root, members in trees.items():
        present = [m for m in members if m in tasks]
        if root in tasks:
            target, outside = root, False
        elif present:
            # The tree starts outside the claim window (its root ran only
            # before it): keep its tasks in one row, excluded like a task
            # with runs before the window.
            def has_present_ancestor(task):
                seen = {task}
                while task in parent and parent[task] not in seen:
                    task = parent[task]
                    seen.add(task)
                    if task in tasks:
                        return True
                return False
            target = min(m for m in present if not has_present_ancestor(m))
            outside = True
        else:
            continue
        t = tasks[target]
        for m in present:
            if m == target:
                continue
            child = tasks.pop(m)
            t["runs"] += child["runs"]
            t["first_claim"] = min(t["first_claim"], child["first_claim"])
            t["excluded"] = t["excluded"] or child["excluded"]
            for name, counts in child["comp"].items():
                t["comp"][name].update(counts)
        if outside:
            t["excluded"] = True
    return tasks


def summarize_tasks(path, comps):
    """Print the task-level baseline the plan quotes: per group, p50 / p90 of
    each component over the tasks that miss none of it, with that n."""
    with open(path) as f:
        every = list(csv.DictReader(f))
    rows = [r for r in every if r["excluded_earlier_runs"] == "False"]
    def hours(r):
        return (ts(r["ended_at"]) - ts(r["first_claim"])).total_seconds() / 3600
    groups = {
        "all": rows,
        "short": [r for r in rows if r["end"] == "completed" and r["runs"] == "1"
                  and r["turn_started"] == "1" and r["review_nonpass"] == "0"],
        "candidate": [r for r in rows if r["candidate"] == "True"],
    }
    print("tasks", len(rows), "ends", dict(collections.Counter(r["end"] for r in rows)),
          "excluded (runs before the window)",
          sum(1 for r in every if r["excluded_earlier_runs"] == "True"))
    for name, g in groups.items():
        done = [r for r in g if r["end"] in DONE]
        out = {"n": len(g), "runs>1": sum(1 for r in g if int(r["runs"]) > 1),
               "hours": [round(x, 2) if x is not None else None
                         for x in (pct([hours(r) for r in done], .5), pct([hours(r) for r in done], .9))],
               "achieved_hours": [round(x, 2) if x is not None else None for x in (
                   pct([hours(r) for r in g if r["end"] in ACHIEVED], .5),
                   pct([hours(r) for r in g if r["end"] in ACHIEVED], .9))],
               "turns": [pct([int(r["turn_started"]) for r in g], q) for q in (.5, .9)],
               "wait_min": [pct([int(r["human_wait_secs"]) // 60 for r in g], q) for q in (.5, .9)]}
        for c in comps:
            have_tok = [int(r[c + "_output_tokens"]) for r in g if r[c + "_tokens_missing"] == "0"
                        and int(r[c + "_sessions"]) > 0]
            have_cost = [float(r[c + "_cost_usd"]) for r in g if r[c + "_cost_missing"] == "0"
                         and int(r[c + "_sessions"]) > 0]
            out[c] = dict(sessions=[pct([int(r[c + "_sessions"]) for r in g], q) for q in (.5, .9)],
                          tok_n=len(have_tok), tok=[pct(have_tok, q) for q in (.5, .9)],
                          cost_n=len(have_cost), cost=[pct(have_cost, q) for q in (.5, .9)])
        print("task group", name, json.dumps(out))


def task_windows(task, cutoffs):
    events = dagq("events", "--task", str(task), "--all", "--full", "--limit", "1000000")["events"]
    for cut in cutoffs:
        upto = [e for e in events if e["id"] <= cut]
        by_run = collections.defaultdict(collections.Counter)
        for e in upto:
            if e.get("run_id") and e["kind"] in ("turn_started", "turn_finished"):
                by_run[e["run_id"][:8]][e["kind"]] += 1
            if e.get("run_id") and e["kind"] == "review_finished":
                by_run[e["run_id"][:8]]["review_" + e["payload"].get("verdict", "?")] += 1
        last = upto[-1]
        print(f"window <= {cut} (last event {last['id']} {last['created_at']}):",
              {k: dict(v) for k, v in by_run.items()})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--claims-from")
    ap.add_argument("--claims-to")
    ap.add_argument("--events-to")
    ap.add_argument("--out", default="out")
    ap.add_argument("--task-windows", type=int)
    ap.add_argument("--cutoff", type=int, action="append", default=[])
    ap.add_argument("--replaced", action="append", default=[],
                    help="SRC:C1,C2 - a replaced task and the children it needs")
    a = ap.parse_args()
    if a.task_windows:
        task_windows(a.task_windows, a.cutoff or [10**12])
        return
    args = ["events", "--all", "--full", "--since", a.claims_from, "--until", a.events_to,
            "--limit", "1000000"]
    for k in KINDS:
        args += ["--kind", k]
    events = dagq(*args)["events"]
    stats = {r["run_id"]: r for r in dagq("stats", "--full", "--since", a.claims_from + "T00:00:00Z",
                                           "--until", a.events_to + "T00:00:00Z")["runs"]}
    replaced = parse_replaced(a.replaced)
    runs = classify(events, a.claims_from, a.claims_to, replaced)
    sessions = dagq("events", "--all", "--full", "--kind", "session_closed", "--since", a.claims_from,
                    "--until", a.events_to, "--limit", "1000000")["events"]
    earlier = {e["task_id"] for e in dagq("events", "--all", "--full", "--kind", "run_claimed",
                                           "--until", a.claims_from, "--limit", "1000000")["events"]}
    tasks = task_level(runs, events, sessions, earlier, a.events_to, replaced)
    os.makedirs(a.out, exist_ok=True)
    causes = ["initial", "review", "conflict", "e2e", "verify", "receipt",
              "worker_question", "provider_host", "unlabeled"]
    with open(os.path.join(a.out, "runs.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["run", "task", "provider", "change", "areas", "end", "claim", "ended_at",
                    "turn_started", "turn_finished", *causes, "nonpass", "pass",
                    "best_nonpass_streak", "detect_event", "turns_after_detect",
                    "nonpass_after_detect", "human_wait_secs", "human_wait_missing",
                    "cost_usd", "cost_missing_turns", "output_tokens", "tokens_missing_turns",
                    "receipt_rejected", "inherited", "population", "worker_mode"])
        for rid, r in sorted(runs.items(), key=lambda kv: kv[1]["claim"]):
            s = stats.get(rid, {})
            r["change"], r["areas"] = s.get("change"), s.get("areas") or []
            end = r["end"] or ("open", "")
            w.writerow([rid[:8], r["task"], r["provider"], s.get("change"), " ".join(s.get("areas") or []), end[0], r["claim"],
                        end[1], r["started"], r["finished"], *[r["turns"][c] for c in causes],
                        sum(v for k, v in r["verdicts"].items() if k != "pass"),
                        r["verdicts"]["pass"], r["best_nonpass_streak"], r["detect"],
                        r["turns_after_detect"], r["nonpass_after_detect"],
                        int(r["human_wait_secs"]), r["human_wait_missing"], round(r["cost"], 2),
                        r["cost_missing"], r["out"], r["tok_missing"], r["receipt_rejected"],
                        r["inherited"], r["population"], r["worker_mode"]])
    comps = ["worker", "review_job", "recovery_job", "planning", "other"]
    with open(os.path.join(a.out, "tasks.csv"), "w", newline="") as f:
        w = csv.writer(f)
        head = ["task", "excluded_earlier_runs", "end", "first_claim", "ended_at", "runs",
                "runs_outside_population",
                "turn_started", "review_nonpass", "candidate", "human_wait_secs",
                "human_wait_missing"]
        for c in comps:
            head += [c + "_sessions", c + "_output_tokens", c + "_tokens_missing",
                     c + "_cost_usd", c + "_cost_missing"]
        w.writerow(head)
        for t in sorted(tasks.values(), key=lambda t: t["first_claim"]):
            end = t.get("end", (t.get("unresolved", "open"), ""))
            rs = t["runs"]
            row = [t["task"], t["excluded"], end[0], t["first_claim"], end[1], len(rs),
                   sum(1 for r in rs if not r["population"]),
                   sum(r["started"] for r in rs),
                   sum(v for r in rs for k, v in r["verdicts"].items() if k != "pass"),
                   any(r["detect"] for r in rs), int(sum(r["human_wait_secs"] for r in rs)),
                   sum(r["human_wait_missing"] for r in rs)]
            for c in comps:
                x = t["comp"][c]
                row += [x["sessions"], x["output_tokens"], x["tokens_missing"],
                        round(x["cost_usd_milli"] / 1000, 2), x["cost_missing"]]
            w.writerow(row)
    summarize_tasks(os.path.join(a.out, "tasks.csv"), comps)
    rows = [r for r in runs.values() if r["population"]]
    print("runs outside the population added to their tasks",
          sorted((rid[:8], r["task"], r["worker_mode"], r["claim"]) for rid, r in runs.items()
                 if not r["population"]))
    total = collections.Counter()
    for r in rows:
        total.update(r["turns"])
    print("runs", len(rows), "ends", collections.Counter((r["end"] or ("open",))[0] for r in rows))
    print("turn causes", dict(total), "sum", sum(total.values()))
    turns = [r["started"] for r in rows]
    print("turns p50/p75/p90/p95/max", [pct(turns, q) for q in (.5, .75, .9, .95, 1)])
    print("nonpass streak hist", sorted(collections.Counter(r["best_nonpass_streak"] for r in rows).items()))
    det = [r for r in rows if r["detect"]]
    print("candidates (3 consecutive nonpass)", len(det),
          "nonpass after detect", sorted(collections.Counter(r["nonpass_after_detect"] for r in det).items()))
    short = [r for r in rows if r["end"] and r["end"][0] == "integrated" and r["started"] == 1
             and not any(k != "pass" for k in r["verdicts"])]
    print("short normal runs", len(short))
    for key in ("change", "areas"):
        groups = collections.defaultdict(list)
        for r in rows:
            for g in (r[key] if key == "areas" else [r[key]]) or ["(none)"]:
                groups[g].append(r)
        for g, members in sorted(groups.items(), key=lambda kv: -len(kv[1])):
            t = [m["started"] for m in members]
            print(key, g, len(members), "turns p50/p90/max", pct(t, .5), pct(t, .9), max(t),
                  "candidates", sum(1 for m in members if m["detect"]))


if __name__ == "__main__":
    sys.exit(main())
