#!/usr/bin/env python3
"""broker の e2e の image のキャッシュ（task 1451）の前後の e2e の測定。

定義と再計算の手順は docs/plans/broker-e2e-image-cache.md。読むのは本番 queue の
`dagq events --kind ... --full`（--since・--until と --after でページング）と、
event の `log` が指す e2e の log（材料）だけで、queue の DB は開かない。

usage: python3 measure.py OUT_DIR   （DAGQ で dagq の path を変えられる）
"""
import csv
import json
import math
import os
import re
import subprocess
import sys

# 期間の境界（UTC）。前 = 1451 の run_integrated の直近 3 日前から、
# 後A = 1451 の run_integrated から 841 の run_integrated まで、後B = そこから T_END まで。
T_1451 = "2026-10-04T19:03:49.783Z"
T_841 = "2026-10-05T02:54:07.372Z"
T_START = "2026-10-01T19:03:49.783Z"
T_END = "2026-10-07T12:00:00.000Z"
TIMEOUT_SECS = 1800

DAGQ = os.environ.get("DAGQ", "dagq")
TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b")
SLOW_LINE = re.compile(r"^test (\S+) has been running for over 60 seconds")
RESULT_LINE = re.compile(
    r"^test result: \w+\. (\d+) passed; (\d+) failed;.*finished in ([0-9.]+)s")


def events(kind):
    after = 0
    out = []
    while True:
        raw = subprocess.run(
            [DAGQ, "events", "--kind", kind, "--full", "--since", T_START,
             "--until", T_END, "--after", str(after), "--limit", "500"],
            check=True, capture_output=True, text=True).stdout
        page = json.loads(raw)["events"]
        if not page:
            return out
        out.extend(page)
        after = page[-1]["id"]


def period(ts):
    if ts < T_1451:
        return "before"
    if ts < T_841:
        return "afterA"
    return "afterB"


def read_log(path):
    """log 由来の列。log が無い・test result の行が無いときは状態だけ返す。"""
    if not path or not os.path.exists(path):
        return {"log_state": "missing"}
    finished, tests_run, last, slow, broker = None, None, None, [], []
    with open(path, errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            m = TEST_LINE.match(line)
            if m and m.group(2) != "ignored":
                last = m.group(1)
                if m.group(1).startswith("broker::") and m.group(1) not in broker:
                    broker.append(m.group(1))
            m = SLOW_LINE.match(line)
            if m and m.group(1) not in slow:
                slow.append(m.group(1))
            m = RESULT_LINE.match(line)
            if m:
                tests_run = int(m.group(1)) + int(m.group(2))
                finished = float(m.group(3))
    if finished is None:
        return {"log_state": "no_test_result"}
    return {
        "log_state": "read",
        "finished_in": finished,
        "tests_run": tests_run,
        "broker_tests": " ".join(broker),
        "broker_count": len(broker),
        "last_test": last or "",
        "broker_last": int(bool(last and last.startswith("broker::"))),
        "slow_tests": " ".join(slow),
        "broker_slow": int(any(t.startswith("broker::") for t in slow)),
    }


def median(xs):
    xs = sorted(xs)
    n = len(xs)
    if n == 0:
        return ""
    return xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2


def p90(xs):
    xs = sorted(xs)
    return xs[math.ceil(0.9 * len(xs)) - 1] if xs else ""


def ratio(num, den):
    return f"{num}/{den}" if den else "0/0"


LOG_COLS = ["log_state", "finished_in", "tests_run", "broker_tests", "broker_count",
            "last_test", "broker_last", "slow_tests", "broker_slow"]


def rel(log):
    """queue の dir からの相対 path（runs/<run>/e2e-N.log・logs/update-*.e2e.log）。"""
    m = re.search(r"/((?:runs|logs)/.*)$", log or "")
    return m.group(1) if m else (log or "")


def write(path, rows, cols):
    with open(path, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, extrasaction="ignore")
        w.writeheader()
        for r in rows:
            w.writerow({c: ("欠測" if c in LOG_COLS[1:] and r["log_state"] != "read"
                            else rel(r["log"]) if c == "log" else r.get(c, "")) for c in cols})


# 部分集合。all は event の全件、broker_ran・no_broker は log が読めて broker の e2e が
# 流れた・流れなかった回だけ（log の読めない回はどちらにも入らない）。
SUBSETS = {
    "all": lambda r: True,
    "broker_ran": lambda r: r["log_state"] == "read" and r["broker_count"] > 0,
    "no_broker": lambda r: r["log_state"] == "read" and r["broker_count"] == 0,
}


def num(x):
    """小数 2 桁に丸め、整数の値は整数で書く（139.0 と 121 を並べない）。"""
    if isinstance(x, float):
        x = round(x, 2)
        return int(x) if x.is_integer() else x
    return x


def summarize(rows, with_wait):
    out = []
    for p, (sub, keep) in [(p, kv) for p in ["before", "afterA", "afterB"] for kv in SUBSETS.items()]:
        rs = [r for r in rows if r["period"] == p and keep(r)]
        secs = [r["secs"] for r in rs if r["secs"] != ""]
        read = [r for r in rs if r["log_state"] == "read"]
        fin = [r["finished_in"] for r in read]
        s = {
            "period": p, "subset": sub, "events": len(rs),
            "secs_n": len(secs), "secs_median": median(secs), "secs_p90": p90(secs),
            "secs_max": max(secs) if secs else "",
            "timeouts": sum(r["timeout"] for r in rs),
            "log_missing": sum(r["log_state"] == "missing" for r in rs),
            "log_no_test_result": sum(r["log_state"] == "no_test_result" for r in rs),
            "log_read": len(read),
            "broker_not_run": sum(r["broker_count"] == 0 for r in read),
            "finished_in_median": median(fin), "finished_in_p90": p90(fin),
            "finished_in_max": max(fin) if fin else "",
            "broker_last": ratio(sum(r["broker_last"] for r in read), len(read)),
            "broker_slow": ratio(sum(r["broker_slow"] for r in read), len(read)),
        }
        if with_wait:
            # lock_wait_secs の無い event（e2e を始められなかった回）は待ちの分母から外す
            waits = [r["lock_wait_secs"] for r in rs if r["lock_wait_secs"] != ""]
            waited = [w for w in waits if w >= 1]
            s.update({
                "wait_n": len(waits), "wait_missing": len(rs) - len(waits),
                "wait_median": median(waits), "wait_p90": p90(waits),
                "wait_max": max(waits) if waits else "",
                "waited": ratio(len(waited), len(waits)),
                "wait_median_of_waited": median(waited),
            })
        out.append({k: num(v) for k, v in s.items()})
    return out


def main():
    out_dir = sys.argv[1]
    runs = []
    for e in events("run_e2e_finished"):
        p = e["payload"]
        r = {"time": e["created_at"], "event_id": e["id"], "period": period(e["created_at"]),
             "run_id": e["run_id"], "task_id": e["task_id"], "attempt": p.get("attempt"),
             "secs": p["secs"], "lock_wait_secs": p.get("lock_wait_secs", ""),
             "outcome": p.get("outcome"),
             "timeout": int(bool(p.get("timed_out")) or p["secs"] >= TIMEOUT_SECS),
             "log": p.get("log", "")}
        r.update(read_log(r["log"]))
        runs.append(r)
    gates = []
    for kind in ["update_e2e_passed", "update_failed"]:
        for e in events(kind):
            p = e["payload"]
            if kind == "update_failed" and p.get("stage") != "e2e":
                continue
            log = p.get("log", "")
            if log.endswith(".build.log"):  # update_failed は build の log を指す
                log = log[: -len(".build.log")] + ".e2e.log"
            r = {"time": e["created_at"], "event_id": e["id"], "period": period(e["created_at"]),
                 "kind": kind, "commit": p.get("commit", ""),
                 "secs": p["secs"] if p.get("secs") is not None else "",
                 "timeout": int((p.get("secs") or 0) >= TIMEOUT_SECS), "log": log}
            r.update(read_log(log))
            gates.append(r)
    gates.sort(key=lambda r: r["time"])
    write(os.path.join(out_dir, "runs.csv"), runs,
          ["time", "event_id", "period", "run_id", "task_id", "attempt", "secs",
           "lock_wait_secs", "timeout", "outcome"] + LOG_COLS + ["log"])
    write(os.path.join(out_dir, "update-gates.csv"), gates,
          ["time", "event_id", "period", "kind", "commit", "secs", "timeout"] + LOG_COLS + ["log"])
    for name, rows, wait in [("summary.csv", runs, True), ("update-gates-summary.csv", gates, False)]:
        s = summarize(rows, wait)
        with open(os.path.join(out_dir, name), "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(s[0].keys()))
            w.writeheader()
            w.writerows(s)


if __name__ == "__main__":
    main()
