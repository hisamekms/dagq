#!/usr/bin/env python3
"""Compute the acceptance-check baseline tables from a snapshot (task 1422).

Reads only SNAPSHOT/normalized/ (events.jsonl, tasks.jsonl, commits.jsonl,
areas.toml). Never reads the queue, SNAPSHOT/reference/ or crosscheck.csv.

  python3 compute.py SNAPSHOT --interval NAME=START,END ... --terciles-from NAME \
      [--changed-task ID ...] [--out DIR]

An interval is [START, END) in UTC; NAME=START,END,CUT also leaves out the
events created at or after CUT (only to repeat an earlier count). A reviewed run of an interval is a run
whose first `review_finished` with a verdict (pass, revise or concern) is
created in it. Writes DIR/runs.csv (one row per interval and run) and
DIR/table.csv (one row per interval and layer).
"""

import argparse
import csv
import json
import math
import os
import re
from datetime import datetime, timezone

VERDICTS = ("pass", "revise", "concern")
UNLANDED = "未着地"
IN_REVIEW = "review 中"
MISSING = "未取得"
UNFINISHED = "未完"


def ms(text):
    """Unix milliseconds of an RFC 3339 UTC time (with or without .fff)."""
    text = text.rstrip("Z")
    fmt = "%Y-%m-%dT%H:%M:%S.%f" if "." in text else "%Y-%m-%dT%H:%M:%S"
    return int(datetime.strptime(text, fmt).replace(tzinfo=timezone.utc).timestamp() * 1000 + 0.5)


def load_jsonl(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


def load_areas(path):
    areas = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#") or line.startswith("["):
                continue
            name, value = line.split("=", 1)
            areas.append((name.strip().strip('"'), json.loads(value.strip())))
    return areas


# The glob rules of src/domain/scope.rs: `*` and `?` inside one segment,
# a segment that is exactly `**` matches zero or more whole segments.
def segment_matches(glob, text):
    if not glob:
        return not text
    if glob[0] == "*":
        return any(segment_matches(glob[1:], text[i:]) for i in range(len(text) + 1))
    if glob[0] == "?":
        return bool(text) and segment_matches(glob[1:], text[1:])
    return bool(text) and text[0] == glob[0] and segment_matches(glob[1:], text[1:])


def segments_match(glob, path):
    if not glob:
        return not path
    if glob[0] == "**":
        return any(segments_match(glob[1:], path[i:]) for i in range(len(path) + 1))
    return bool(path) and segment_matches(glob[0], path[0]) and segments_match(glob[1:], path[1:])


def glob_matches(glob, path):
    return segments_match(glob.split("/"), path.split("/"))


def areas_of(files, table):
    names = set()
    for path in files:
        hit = [name for name, globs in table if any(glob_matches(g, path) for g in globs)]
        names.update(hit or ["other"])
    return sorted(names)


# The landing clock of src/domain/stats/landing.rs (LandClock), for the
# `ask` phase of land_phases: from the first validation_finished to
# run_integrated, each listed event moves the clock to its phase.
class LandClock:
    def __init__(self, event, at):
        self.phase, self.since = "exit", at
        self.spent = {}
        self.before_ask = None
        self.asks = {}
        self.integrated = None
        self.observe(event, at)

    @staticmethod
    def ask_key(payload):
        key = payload.get("ask_id", payload.get("id"))
        return None if key is None else (key if isinstance(key, str) else json.dumps(key))

    def observe(self, event, at):
        kind, payload = event["kind"], event["payload"] or {}
        if self.integrated is not None:
            return
        if kind == "run_integrated":
            self.enter(self.phase, at)
            self.integrated = at
            return
        nxt = None
        if payload.get("status") == "needs_session":
            nxt = "resume"
        elif kind in ("review_started", "review_retried"):
            nxt = "review"
        elif kind in ("review_finished", "validation_finished", "revise_unsent"):
            nxt = "exit"
        elif kind == "review_failed":
            nxt = "ask"
        elif kind == "revise_requested":
            nxt = "revise"
        elif kind == "conflict_precheck":
            nxt = "conflict" if payload.get("requested") is True else "exit"
        elif kind in ("landing_queued", "run_e2e_finished"):
            nxt = "landing_queue"
        elif kind == "run_e2e_waiting":
            nxt = "e2e_wait"
        elif kind == "run_e2e_started":
            nxt = "e2e"
        elif kind in ("integration_error", "integration_held"):
            nxt = "ask"
        elif kind == "integration_started":
            nxt = "rebase"
        elif kind == "integration_rebased":
            nxt = "verify"
        elif kind == "ask_opened":
            if payload.get("kind") not in ("blocked", "planner_question"):
                key = self.ask_key(payload)
                if key is not None:
                    self.asks[key] = payload.get("kind")
                if self.phase != "ask":
                    self.before_ask = self.phase
                nxt = "ask"
        elif kind == "ask_answered":
            key = self.ask_key(payload)
            opened = self.asks.pop(key, None) if key is not None else None
            answered = payload.get("kind") or opened
            if answered == "approve_landing":
                self.asks.clear()
                self.before_ask = None
                nxt = "ask" if payload.get("runtime_delivers") is False else "landing_queue"
            elif not self.asks:
                nxt, self.before_ask = self.before_ask, None
        if nxt is not None:
            if nxt != "ask" and kind != "ask_answered":
                self.asks.clear()
                self.before_ask = None
            self.enter(nxt, at)

    def enter(self, phase, at):
        self.spent[self.phase] = self.spent.get(self.phase, 0) + max(0, at - self.since)
        self.since = max(at, self.since)
        self.phase = phase

    def ask_secs(self):
        return self.spent.get("ask", 0) // 1000


def overlap(spans, start, end):
    """Milliseconds of the closed waits inside [start, end]; None when an
    unfinished wait starts before end (it is not cut at C)."""
    total = 0
    for s, e in spans:
        if e is None:
            if s < end:
                return None
            continue
        total += max(0, min(e, end) - max(s, start))
    return total


def run_rows(events, tasks, commits, area_table, changed_from):
    by_run = {}
    for e in events:
        if e.get("run_id"):
            by_run.setdefault(e["run_id"], []).append(e)
    runs_of_task = {}
    for e in events:
        if e["kind"] == "run_claimed" and e.get("run_id"):
            runs_of_task.setdefault(e["task_id"], set()).add(e["run_id"])
    rows = []
    for run_id, evs in by_run.items():
        evs.sort(key=lambda e: e["id"])
        verdicts = [e for e in evs if e["kind"] == "review_finished"
                    and (e["payload"] or {}).get("verdict") in VERDICTS]
        failed_reviews = [e for e in evs if e["kind"] == "review_failed"]
        if not verdicts and not failed_reviews:
            continue
        task_id = evs[0]["task_id"]
        task = tasks.get(task_id, {})
        first = lambda kind: next((e for e in evs if e["kind"] == kind), None)
        claim, receipt = first("run_claimed"), first("receipt_observed")
        validated, landed = first("validation_finished"), first("run_integrated")
        switches = [e for e in evs if e["kind"] == "provider_switched"]
        starts = [e for e in evs if e["kind"] == "review_started"]
        # Ended: the run's last recorded status (before C) is failed or
        # interrupted; an earlier failed that a resume took back does not count.
        statuses = [e for e in evs if (e["payload"] or {}).get("status")]
        terminal = statuses[-1] if statuses and statuses[-1]["payload"]["status"] in ("failed", "interrupted") else None

        waits, open_wait = [], None
        for e in evs:
            if e["kind"] == "run_waiting_started" and open_wait is None:
                open_wait = ms(e["created_at"])
            elif e["kind"] == "run_waiting_ended" and open_wait is not None:
                waits.append((open_wait, ms(e["created_at"])))
                open_wait = None
        if open_wait is not None:
            waits.append((open_wait, None))

        row = {"run_id": run_id, "task_id": task_id}
        row["change"] = task.get("change") or "unknown"
        row["items"] = task.get("items") if task.get("items") is not None else MISSING
        row["route"] = (claim["payload"].get("worker_mode") if claim else None) or MISSING
        if switches:
            row["actual_provider"] = switches[-1]["payload"].get("to") or MISSING
        else:
            row["actual_provider"] = (claim["payload"].get("provider") if claim else None) or MISSING
        providers = [((s["payload"] or {}).get("launch") or {}).get("provider") for s in starts]
        row["review_provider"] = (providers[0] if providers else None) or MISSING
        row["review_providers"] = ";".join(sorted({p for p in providers if p}))
        row["first_review_at"] = verdicts[0]["created_at"] if verdicts else ""
        row["verdicts"] = ">".join(v["payload"]["verdict"] for v in verdicts)
        row["review_failed"] = len(failed_reviews)
        codes = [v["payload"].get("reason_codes") or [] for v in verdicts]
        au = [any("acceptance_unmet" in item for item in c) for c in codes]
        row["acceptance_unmet"] = int(any(au))
        row["acceptance_unmet_verdicts"] = sum(au)
        row["acceptance_unmet_primary"] = sum(1 for v in verdicts if v["payload"].get("primary_code") == "acceptance_unmet")
        primaries = []
        for v in verdicts:
            p = v["payload"]
            if p["verdict"] == "pass":
                continue
            code = p.get("primary_code")
            if not code:
                flat = [c for item in (p.get("reason_codes") or []) for c in item]
                code = flat[0] if flat else "unlabeled"
            primaries.append(code)
        row["primary_codes"] = ";".join(primaries)

        # work: first run_claimed -> first receipt_observed.
        work = excl = None
        if claim and receipt:
            c, r = ms(claim["created_at"]), ms(receipt["created_at"])
            work = (r - c) // 1000
            o = overlap(waits, c, r)
            excl = None if o is None else (r - c - o) // 1000
        row["work"] = MISSING if work is None else work
        row["work_excl_wait"] = MISSING if work is None else (UNFINISHED if excl is None else excl)

        # review: every review_finished and review_failed's duration_secs.
        durations = [(e["payload"] or {}).get("duration_secs") for e in verdicts + failed_reviews]
        spans = []
        for s in starts:
            att = s["payload"].get("attempt")
            end = next((e for e in evs if e["id"] > s["id"] and e["kind"] in ("review_finished", "review_failed")
                        and (e["payload"] or {}).get("attempt") == att), None)
            if end:
                spans.append((ms(s["created_at"]), ms(end["created_at"])))
        if any(d is None for d in durations):
            row["review"] = row["review_excl_wait"] = MISSING
        else:
            total = int(round(sum(durations)))
            row["review"] = total
            waited = 0
            for a, b in spans:
                o = overlap(waits, a, b)
                if o is None:
                    waited = None
                    break
                waited += o
            row["review_excl_wait"] = UNFINISHED if waited is None else max(0, total - waited // 1000)

        last_review = max([e["id"] for e in verdicts + failed_reviews])
        in_review = (verdicts and verdicts[-1]["payload"]["verdict"] != "pass"
                     and verdicts[-1]["id"] == last_review and landed is None and terminal is None)
        row["state"] = "landed" if landed else (IN_REVIEW if in_review else
                                                ("ended" if terminal else UNLANDED))

        # wait_to_land: first validation_finished -> run_integrated, and
        # without land_phases.ask (the waits for a person's answer).
        if landed and validated:
            clock = None
            for e in evs:
                at = ms(e["created_at"])
                if clock is None:
                    if e["id"] == validated["id"]:
                        clock = LandClock(e, at)
                else:
                    clock.observe(e, at)
            wtl = (ms(landed["created_at"]) - ms(validated["created_at"])) // 1000
            row["wait_to_land"] = wtl
            row["land_ask"] = clock.ask_secs()
            row["wait_to_land_excl_ask"] = wtl - clock.ask_secs()
        else:
            row["wait_to_land"] = row["land_ask"] = row["wait_to_land_excl_ask"] = UNLANDED

        commit = None
        if landed:
            commit = commits.get(landed["payload"].get("commit") or landed["payload"].get("result_commit"))
        if commit and commit["found"]:
            row["lines"] = commit["added"] + commit["deleted"]
            row["areas"] = ";".join(areas_of(commit["files"], area_table))
        else:
            row["lines"] = UNLANDED if not landed else MISSING
            row["areas"] = ""

        resumes = [e for e in evs if e["kind"] in ("resume_started", "revise_requested", "integration_deferred")]
        row["kpi_first_pass"] = int(bool(landed) and not resumes and len(runs_of_task.get(task_id, ())) == 1)
        later = [e for e in evs if changed_from is not None and ms(e["created_at"]) >= changed_from
                 and e["kind"] in ("review_started", "review_finished", "review_failed", "resume_started")]
        row["after_1420_1421"] = int(bool(later))
        rows.append(row)
    return rows


def median(values):
    v = sorted(values)
    if not v:
        return ""
    n = len(v)
    return v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) // 2


def p90(values):
    v = sorted(values)
    return v[math.ceil(0.9 * len(v)) - 1] if v else ""


def rate(a, b):
    return "" if b == 0 else "%.3f" % (a / b)


def bins(rows, boundaries):
    lo, hi = boundaries
    for r in rows:
        i = r["items"]
        r["items_bin"] = MISSING if i == MISSING else ("1-3" if i <= 3 else ("4-6" if i <= 6 else "7+"))
        x = r["lines"]
        if not isinstance(x, int):
            r["lines_bin"] = x
        else:
            r["lines_bin"] = ("<=%d" % lo) if x <= lo else (("%d-%d" % (lo + 1, hi)) if x <= hi else (">%d" % hi))


def terciles(rows):
    v = sorted(r["lines"] for r in rows if isinstance(r["lines"], int))
    n = len(v)
    return v[math.ceil(n / 3) - 1], v[math.ceil(2 * n / 3) - 1]


def layers(r):
    out = [("all", "all"), ("change", r["change"]), ("items", r["items_bin"]),
           ("lines", r["lines_bin"]), ("review_provider", r["review_provider"]),
           ("worker", "%s/%s" % (r["route"], r["actual_provider"]))]
    out += [("area", a) for a in (r["areas"].split(";") if r["areas"] else ["unknown"])]
    return out


def table_row(interval, layer, value, rows):
    n = len(rows)
    vs = [v for r in rows for v in r["verdicts"].split(">") if v]
    prim = {}
    for r in rows:
        for c in filter(None, r["primary_codes"].split(";")):
            prim[c] = prim.get(c, 0) + 1
    out = {"interval": interval, "layer": layer, "value": value, "n": n}
    fp = sum(1 for r in rows if r["verdicts"].split(">")[0] == "pass")
    sb = sum(1 for r in rows if any(v in ("revise", "concern") for v in r["verdicts"].split(">")))
    out.update(first_pass=fp, first_pass_rate=rate(fp, n), sent_back=sb, sent_back_rate=rate(sb, n),
               verdicts_pass=vs.count("pass"), verdicts_revise=vs.count("revise"),
               verdicts_concern=vs.count("concern"),
               au_runs=sum(r["acceptance_unmet"] for r in rows),
               au_verdicts=sum(r["acceptance_unmet_verdicts"] for r in rows),
               au_primary=sum(r["acceptance_unmet_primary"] for r in rows),
               au_primary_runs=sum(1 for r in rows if r["acceptance_unmet_primary"]),
               primary_codes=";".join("%s:%d" % (k, prim[k]) for k in sorted(prim)),
               reviews_per_run="" if n == 0 else "%.2f" % (len(vs) / n))
    reviewing = [r for r in rows if r["state"] != IN_REVIEW]
    for col, pool in (("work", rows), ("work_excl_wait", rows), ("review", reviewing),
                      ("review_excl_wait", reviewing), ("wait_to_land", rows),
                      ("wait_to_land_excl_ask", rows)):
        vals = [r[col] for r in pool if isinstance(r[col], int)]
        out[col + "_median"], out[col + "_p90"] = median(vals), p90(vals)
    out["unlanded"] = sum(1 for r in rows if r["state"] != "landed")
    out["in_review"] = sum(1 for r in rows if r["state"] == IN_REVIEW)
    out["missing"] = sum(1 for r in rows if any(r[k] in (MISSING, UNFINISHED) for k in
                         ("items", "route", "actual_provider", "review_provider", "work",
                          "work_excl_wait", "review", "review_excl_wait", "lines")))
    landed = [r for r in rows if r["state"] == "landed"]
    kfp = sum(r["kpi_first_pass"] for r in landed)
    out.update(landed=len(landed), kpi_first_pass=kfp, kpi_first_pass_rate=rate(kfp, len(landed)))
    return out


RUN_COLUMNS = ["interval", "run_id", "task_id", "change", "areas", "items", "items_bin", "lines", "lines_bin",
               "review_provider", "review_providers", "route", "actual_provider", "first_review_at", "verdicts",
               "review_failed", "acceptance_unmet", "acceptance_unmet_verdicts", "acceptance_unmet_primary", "primary_codes", "state",
               "work", "work_excl_wait", "review", "review_excl_wait", "wait_to_land", "land_ask",
               "wait_to_land_excl_ask", "kpi_first_pass", "after_1420_1421"]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("snapshot")
    p.add_argument("--interval", action="append", required=True,
                   help="NAME=START,END[,CUT] (UTC, [START, END); CUT < C)")
    p.add_argument("--terciles-from", default=None, help="the interval whose landed runs set the line terciles")
    p.add_argument("--line-terciles", default=None,
                   help="LOW,HIGH: fixed line terciles (a baseline's meta.json) instead of --terciles-from")
    p.add_argument("--changed-task", action="append", type=int, default=[],
                   help="a task whose first landing marks the change (1420, 1421)")
    p.add_argument("--out", default=None)
    a = p.parse_args()
    norm = os.path.join(a.snapshot, "normalized")
    out = a.out or os.path.join(os.path.dirname(os.path.abspath(__file__)), "out",
                                os.path.basename(os.path.normpath(a.snapshot)))
    os.makedirs(out, exist_ok=True)
    events = load_jsonl(os.path.join(norm, "events.jsonl"))
    tasks = {t["task_id"]: t for t in load_jsonl(os.path.join(norm, "tasks.jsonl"))}
    commits = {c["commit"]: c for c in load_jsonl(os.path.join(norm, "commits.jsonl"))}
    area_table = load_areas(os.path.join(norm, "areas.toml"))
    landings = [ms(e["created_at"]) for e in events
                if e["kind"] == "run_integrated" and e.get("task_id") in a.changed_task]
    changed_from = min(landings) if landings else None
    intervals, by_cut = [], {}
    for spec in a.interval:
        name, span = spec.split("=", 1)
        start, end, *cut = span.split(",")
        intervals.append((name, ms(start), ms(end), ms(cut[0]) if cut else None))
    member = {}
    for name, start, end, cut in intervals:
        # An interval with its own cut reads only the events before it
        # (to match an earlier count); the others read every event (< C).
        if cut not in by_cut:
            seen = events if cut is None else [e for e in events if ms(e["created_at"]) < cut]
            by_cut[cut] = run_rows(seen, tasks, commits, area_table, changed_from)
        member[name] = sorted((r for r in by_cut[cut] if r["first_review_at"]
                               and start <= ms(r["first_review_at"]) < end), key=lambda r: r["run_id"])
    if a.line_terciles:
        bounds = tuple(int(x) for x in a.line_terciles.split(","))
    else:
        bounds = terciles(member[a.terciles_from])
    for rows in by_cut.values():
        bins(rows, bounds)
    rows = by_cut[None] if None in by_cut else next(iter(by_cut.values()))

    with open(os.path.join(out, "runs.csv"), "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=RUN_COLUMNS, lineterminator="\n")
        w.writeheader()
        for name, _, _, _ in intervals:
            for r in member[name]:
                w.writerow(dict(r, interval=name))
    table = []
    for name, start, end, cut in intervals:
        groups = {}
        for r in member[name]:
            for layer in layers(r):
                groups.setdefault(layer, []).append(r)
        order = ["all", "change", "area", "items", "lines", "review_provider", "worker"]
        for layer, value in sorted(groups, key=lambda k: (order.index(k[0]), str(k[1]))):
            table.append(table_row(name, layer, value, groups[(layer, value)]))
        failed_only = sum(1 for r in by_cut[cut] if not r["verdicts"] and r["review_failed"] and any(
            start <= ms(e["created_at"]) < end and (cut is None or ms(e["created_at"]) < cut) for e in events
            if e.get("run_id") == r["run_id"] and e["kind"] == "review_failed"))
        table.append(dict(interval=name, layer="review_failed_only", value="runs", n=failed_only))
    cols = list(table[0].keys())
    with open(os.path.join(out, "table.csv"), "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=cols, lineterminator="\n")
        w.writeheader()
        w.writerows(table)
    with open(os.path.join(out, "meta.json"), "w", encoding="utf-8") as f:
        json.dump({"line_terciles": list(bounds), "terciles_from": a.terciles_from,
                   "first_landing_of_changed_tasks_ms": changed_from,
                   "intervals": [[n, s, e, c] for n, s, e, c in intervals]}, f, sort_keys=True, indent=1)
        f.write("\n")
    print("runs %d -> %s" % (sum(len(m) for m in member.values()), out))


if __name__ == "__main__":
    main()
