"""run の review の試みの表・同時の数・上限 L の模擬を作る（docs/plans/review-parallel-jobs-measure.md）。

使い方: python3 -I analyze.py <events をまとめた JSON> <attempts.csv の出力先> [<切れ目の UTC 時刻>]
入力は dagq events --full の各ページの .events を jq -s '[.[].events[]]' でまとめた配列。
切れ目を渡さなければ全ての試みを「前」にする。
"""

import csv
import json
import math
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone

W0 = datetime(2026, 10, 1, tzinfo=timezone.utc)
W1 = datetime(2026, 10, 8, tzinfo=timezone.utc)
LIMITS = [2, 4, 6, 8, 10, 12]


def ts(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)


def nearest_rank(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    return xs[max(1, math.ceil(q * len(xs))) - 1]


def weighted_rank(pairs, q):
    """(値, 重み) の組の重みつきの最近順位: 累積の重みが q 以上になる最小の値。"""
    pairs = sorted(pairs)
    total = sum(w for _, w in pairs)
    acc = 0.0
    for v, w in pairs:
        acc += w
        if acc >= q * total - 1e-9:
            return v
    return pairs[-1][0] if pairs else None


def step(intervals, lo, hi):
    """[(始まり, 終わり, 重み)] を [lo, hi) に切り取り、(値, 続いた秒) の組を返す。"""
    edges = []
    for s, e, w in intervals:
        s, e = max(s, lo), min(e, hi)
        if e > s:
            edges.append((s, w))
            edges.append((e, -w))
    edges.sort()
    out = []
    level, t = 0, lo
    for x, d in edges:
        if x > t:
            out.append((level, (x - t).total_seconds()))
            t = x
        level += d
    if hi > t:
        out.append((level, (hi - t).total_seconds()))
    return out


def stats_of(pairs):
    busy = [(v, w) for v, w in pairs if v > 0]
    return {
        "max": max(v for v, _ in pairs),
        "all_median": weighted_rank(pairs, 0.5),
        "all_p90": weighted_rank(pairs, 0.9),
        "busy_median": weighted_rank(busy, 0.5),
        "busy_p90": weighted_rank(busy, 0.9),
        "busy_hours": round(sum(w for _, w in busy) / 3600, 1),
        "all_hours": round(sum(w for _, w in pairs) / 3600, 1),
    }


def simulate(rows, limit):
    """各試みの 1+N 本の job を始まりの時刻に同時に要求し、空きが limit 本の先着順で起動する。"""
    free = [W0 - timedelta(days=1)] * limit
    waits, delays = [], []
    for r in sorted(rows, key=lambda r: (r["start"], r["run_id"])):
        dur = r["proc_end"] - r["start"]
        worst = 0.0
        for _ in range(r["jobs"]):
            i = min(range(limit), key=lambda k: free[k])
            begin = max(r["start"], free[i])
            free[i] = begin + dur
            w = (begin - r["start"]).total_seconds()
            waits.append(w)
            worst = max(worst, w)
        delays.append(worst)
    return waits, delays


def main():
    events = json.load(open(sys.argv[1]))
    cut = ts(sys.argv[3]) if len(sys.argv) > 3 else None
    events.sort(key=lambda e: e["id"])
    opened = {}
    closed = defaultdict(list)
    starts = []
    ends = defaultdict(list)
    for e in events:
        p, k = e["payload"], e["kind"]
        if k == "session_opened":
            opened[e["id"]] = e
        elif k == "session_closed":
            closed[p.get("session_id")].append(e)
        elif k == "review_started" and e["run_id"]:
            starts.append(e)
        elif k in ("review_finished", "review_failed", "review_retried") and e["run_id"]:
            ends[(e["run_id"], p.get("attempt"))].append(e)

    by_run = defaultdict(list)
    for s in starts:
        by_run[s["run_id"]].append(s)
    rows = []
    for s in starts:
        p = s["payload"]
        start = ts(s["created_at"])
        key = (s["run_id"], p.get("attempt"))
        cand = [e for e in ends[key] if ts(e["created_at"]) >= start]
        end = min(cand, key=lambda e: e["id"]) if cand else None
        later = [ts(x["created_at"]) for x in by_run[s["run_id"]] if ts(x["created_at"]) > start]
        nxt = min(later) if later else None

        sid = p.get("session_id")
        close_ok = None
        tokens = None
        for c in closed.get(sid, []):
            cp = c["payload"]
            op = opened.get(cp.get("opened_event_id"))
            if op is None or op["payload"].get("session_id") != sid or op["run_id"] != s["run_id"]:
                continue
            if cp.get("kind") != "review":
                continue
            tokens = tokens or cp.get("tokens")
            if cp.get("reason") == "job_finished" and close_ok is None:
                close_ok = c

        if end is None:
            result = "open"
        elif end["kind"] == "review_finished":
            result = end["payload"].get("verdict")
        elif end["kind"] == "review_failed":
            result = "failed"
        else:
            result = "retried"

        secs = None
        if end is not None:
            if end["kind"] in ("review_finished", "review_failed"):
                secs = end["payload"].get("duration_secs")
            else:
                secs = (ts(end["created_at"]) - start).total_seconds()

        if close_ok is not None:
            proc_end, source = ts(close_ok["created_at"]), "session_closed"
        elif end is not None and end["kind"] in ("review_finished", "review_retried"):
            proc_end, source = ts(end["created_at"]), "recorded"
        elif end is not None and end["kind"] == "review_failed" and secs is not None:
            proc_end, source = start + timedelta(seconds=secs), "estimated"
        elif end is None:
            proc_end, source = min(nxt or W1, W1), "open_closed_at_next_or_window_end"
        else:
            proc_end, source = None, "missing"

        failed_wait = None
        if end is not None and end["kind"] == "review_failed" and close_ok is not None:
            failed_wait = (ts(end["created_at"]) - ts(close_ok["created_at"])).total_seconds()

        sub = (p.get("subagents") or {}).get("agents") or []
        names = [a.get("agent") for a in sub]
        launch = p.get("launch") or {}
        fin_tokens = end["payload"].get("tokens") if end is not None and end["kind"] == "review_finished" else None
        rows.append(
            {
                "run_id": s["run_id"],
                "attempt": p.get("attempt"),
                "start": start,
                "started_at": s["created_at"],
                "start_event": s["id"],
                "end_kind": end["kind"] if end else "",
                "end_event": end["id"] if end else "",
                "secs": secs,
                "result": result,
                "n_agents": len(names),
                "agents": " ".join(names),
                "provider": launch.get("provider"),
                "switched_from": launch.get("switched_from") or "",
                "input": (tokens or {}).get("input"),
                "output": (tokens or {}).get("output"),
                "cache_creation": (tokens or {}).get("cache_creation"),
                "cache_read": (tokens or {}).get("cache_read"),
                "usage_output": (fin_tokens or {}).get("output"),
                "usage_input_total": None
                if not fin_tokens
                else sum(fin_tokens.get(k, 0) for k in ("input", "cache_creation", "cache_read")),
                "proc_end": proc_end,
                "end_source": source,
                "failed_wait": failed_wait,
                "side": "後" if cut and start >= cut else "前",
                "in_window": W0 <= start < W1,
            }
        )

    inw = [r for r in rows if r["in_window"]]
    with open(sys.argv[2], "w", newline="") as f:
        cols = [
            "run_id", "attempt", "started_at", "start_event", "end_kind", "end_event", "secs",
            "result", "n_agents", "agents", "provider", "switched_from", "input", "output",
            "cache_creation", "cache_read", "usage_input_total", "usage_output", "end_source",
            "proc_end", "side",
        ]
        w = csv.writer(f)
        w.writerow(cols)
        for r in sorted(inw, key=lambda r: (r["start"], r["run_id"])):
            row = dict(r)
            row["proc_end"] = r["proc_end"].strftime("%Y-%m-%dT%H:%M:%SZ") if r["proc_end"] else ""
            w.writerow([row[c] if row[c] is not None else "" for c in cols])

    runs = defaultdict(list)
    for r in inw:
        runs[r["run_id"]].append(r)
    with open(sys.argv[2].replace("attempts.csv", "runs.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["run_id", "attempts", "secs_sum", "jobs_sum", "results", "output_sum"])
        for run_id, rs in sorted(runs.items(), key=lambda x: min(r["start"] for r in x[1])):
            rs.sort(key=lambda r: r["attempt"])
            w.writerow([
                run_id, len(rs), round(sum(r["secs"] or 0 for r in rs), 3),
                sum(1 + r["n_agents"] for r in rs), " ".join(r["result"] for r in rs),
                sum(r["output"] or 0 for r in rs),
            ])

    out = {"runs": len(runs)}
    out["attempts_in_window"] = len(inw)
    out["attempts_started_before_window_overlapping"] = sum(
        1 for r in rows if r["start"] < W0 and r["proc_end"] and r["proc_end"] > W0
    )
    rc = defaultdict(int)
    for r in inw:
        rc[r["result"]] += 1
    out["result_counts"] = dict(rc)
    sc = defaultdict(int)
    for r in inw:
        sc[r["end_source"]] += 1
    out["end_source_counts"] = dict(sc)
    out["open_runs"] = sorted({f'{r["run_id"]}#{r["attempt"]}' for r in inw if r["result"] == "open"})
    out["missing_runs"] = sorted({f'{r["run_id"]}#{r["attempt"]}' for r in inw if r["end_source"] == "missing"})
    fw = [r["failed_wait"] for r in inw if r["failed_wait"] is not None]
    out["failed_worker_wait"] = {
        "n": len(fw), "median": nearest_rank(fw, 0.5), "max": max(fw) if fw else None,
    }

    for side in ("前", "後"):
        rs = [r for r in inw if r["side"] == side and r["secs"] is not None]
        d = {"n": len(rs), "median": nearest_rank([r["secs"] for r in rs], 0.5),
             "p90": nearest_rank([r["secs"] for r in rs], 0.9), "by_result": {}}
        for res in ("pass", "revise", "concern", "failed", "retried"):
            xs = [r["secs"] for r in rs if r["result"] == res]
            d["by_result"][res] = {"n": len(xs), "median": nearest_rank(xs, 0.5), "p90": nearest_rank(xs, 0.9)}
        tk = [r for r in inw if r["side"] == side and r["output"] is not None]
        d["tokens"] = {
            "n": len(tk),
            "provider_counts": dict(
                (p, sum(1 for r in inw if r["side"] == side and r["provider"] == p))
                for p in sorted({r["provider"] for r in inw})
            ),
        }
        for k in ("input", "output", "cache_creation", "cache_read"):
            xs = [r[k] for r in tk]
            d["tokens"][k] = {"median": nearest_rank(xs, 0.5), "p90": nearest_rank(xs, 0.9), "sum": sum(xs)}
        for n in sorted({r["n_agents"] for r in tk}):
            xs = [r["output"] for r in tk if r["n_agents"] == n]
            ys = [r["cache_read"] + r["cache_creation"] + r["input"] for r in tk if r["n_agents"] == n]
            d["tokens"][f"by_agents_{n}"] = {
                "n": len(xs), "output_median": nearest_rank(xs, 0.5), "input_total_median": nearest_rank(ys, 0.5),
            }
        both = [r for r in inw if r["side"] == side and r["usage_output"] is not None and r["output"] is not None]
        d["tokens"]["usage_vs_session"] = [
            {"run": r["run_id"], "attempt": r["attempt"], "n_agents": r["n_agents"],
             "usage_out": r["usage_output"], "session_out": r["output"],
             "usage_in": r["usage_input_total"],
             "session_in": r["input"] + r["cache_creation"] + r["cache_read"]}
            for r in both
        ]
        out[f"side_{side}"] = d

    nd = defaultdict(int)
    for r in inw:
        nd[r["n_agents"]] += 1
    out["agents_distribution"] = dict(sorted(nd.items()))
    an = defaultdict(int)
    for r in inw:
        for a in r["agents"].split():
            an[a] += 1
    out["agent_names"] = dict(sorted(an.items(), key=lambda x: -x[1]))
    pv = defaultdict(int)
    for r in inw:
        pv[(r["provider"], r["switched_from"])] += 1
    out["providers"] = {f"{k[0]}<-{k[1] or '-'}": v for k, v in pv.items()}

    usable = [r for r in rows if r["proc_end"] is not None and r["proc_end"] > W0 and r["start"] < W1]
    rev = step([(r["start"], r["proc_end"], 1) for r in usable], W0, W1)
    jobs = step([(r["start"], r["proc_end"], 1 + r["n_agents"]) for r in usable], W0, W1)
    out["concurrency_reviews"] = stats_of(rev)
    out["concurrency_jobs"] = stats_of(jobs)
    # 参考: subagent を選ぶ仕組みが動き始めた後（最初に agents を持つ review_started から窓の終わりまで）
    lo = min(r["start"] for r in inw if r["n_agents"] > 0)
    out["agents_since"] = lo.isoformat()
    out["concurrency_reviews_since_agents"] = stats_of(
        step([(r["start"], r["proc_end"], 1) for r in usable], lo, W1))
    out["concurrency_jobs_since_agents"] = stats_of(
        step([(r["start"], r["proc_end"], 1 + r["n_agents"]) for r in usable], lo, W1))
    rev_levels = defaultdict(float)
    for v, w in rev:
        rev_levels[v] += w / 3600
    out["concurrency_reviews_hours_by_level"] = {k: round(v, 2) for k, v in sorted(rev_levels.items())}
    job_levels = defaultdict(float)
    for v, w in jobs:
        job_levels[v] += w / 3600
    out["concurrency_jobs_hours_by_level"] = {k: round(v, 2) for k, v in sorted(job_levels.items())}

    sim_rows = [dict(r, jobs=1 + r["n_agents"]) for r in inw if r["proc_end"] is not None]
    for r in sim_rows:
        r["proc_end"] = max(r["proc_end"], r["start"])
    out["sim_n_reviews"] = len(sim_rows)
    out["sim_n_jobs"] = sum(r["jobs"] for r in sim_rows)
    sims = {}
    for limit in sorted(set(LIMITS + [out["concurrency_jobs"]["max"]])):
        waits, delays = simulate(sim_rows, limit)
        sims[limit] = {
            "wait_median": nearest_rank(waits, 0.5), "wait_p90": nearest_rank(waits, 0.9),
            "wait_max": max(waits), "jobs_waited": sum(1 for w in waits if w > 0),
            "delay_median": nearest_rank(delays, 0.5), "delay_p90": nearest_rank(delays, 0.9),
            "delay_max": max(delays), "reviews_delayed": sum(1 for d in delays if d > 0),
        }
    out["simulation"] = sims
    sub = [r for r in sim_rows if r["start"] >= lo]
    out["sim_since_agents_n_reviews"] = len(sub)
    sims2 = {}
    for limit in sorted(set(LIMITS + [out["concurrency_jobs"]["max"]])):
        waits, delays = simulate(sub, limit)
        sims2[limit] = {
            "wait_median": nearest_rank(waits, 0.5), "wait_p90": nearest_rank(waits, 0.9),
            "wait_max": max(waits), "delay_median": nearest_rank(delays, 0.5),
            "delay_p90": nearest_rank(delays, 0.9), "delay_max": max(delays),
        }
    out["simulation_since_agents"] = sims2
    fin = [r for r in inw if r["result"] in ("pass", "revise", "concern") and r["provider"] == "claude"]
    out["claude_finished_tokens_missing"] = sum(1 for r in fin if r["output"] is None)
    print(json.dumps(out, ensure_ascii=False, indent=1, default=str))


if __name__ == "__main__":
    main()
