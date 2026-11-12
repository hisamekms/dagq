#!/usr/bin/env python3
"""着地待ちの想定の足し方の当て直し（docs/plans/disk-landing-expectation-spike.md）。

使い方: replay.py DATA_DIR CSV_DIR OUT_JSON
DATA_DIR は collect.sh の JSONL、CSV_DIR は host/metrics-YYYYMMDD.csv の dir。どちらも読むだけ。
同じ入力に決まる計算で、乱数も時刻も使わない。仮定は文書の「仮定」の節に書く。
CSV の読みと欠測（GAP）・hold_spans の区間・容量不足の区間の数え方は
docs/plans/disk-claim-replay-spike/replay.py から写し、閾値の式・期間・動いている run・想定は
対象の commit の domain::disk（DiskState::needs・run_samples・expected_run_size）に合わせて書き直した。
"""
import bisect
import csv
import datetime as dt
import glob
import json
import math
import os
import statistics
import sys

GIB = 2**30
GAP = 300  # 秒。隣の sample との間がこれを超えれば欠測
PARALLEL = 3  # dagq.toml の [supervisor] parallel
LANDING_SLOTS = 1  # 着地 slot は 1 本（domain::run_phase の「the landing slot」、dagq.toml の「integrate 1 本」）
# domain::disk の定数
SAMPLE_WINDOW = 14 * 86400
SAMPLE_COUNT = 20
QUANTILE_SAMPLES = 5
QUANTILE_TENTHS = 9
MIN_RESERVE = 5 * GIB
RESERVE_SHARE = 0.05  # dagq.toml に [disk] は無い
SAMPLE_KINDS = ["build_outputs_removed", "worktree_removed", "scratchpad_removed", "run_tmp_removed"]
PART = {
    "build_outputs_removed": "build",
    "worktree_removed": "build",
    "scratchpad_removed": "scratchpad",
    "run_tmp_removed": "tmp",
}
# 動いている run に数えない phase（ADR-t4473-1 決定4: 人の答え・着地の順番・provider の控え）
NOT_MOVING = {"awaiting_slot", "review_held", "waiting"}
# 着地待ち（supervisor の slot の AwaitingSlot。landing_queue は着地の順番だけを待つ run）
LANDING_WAIT = {"awaiting_slot", "landing_queue"}
WAYS = ["a", "b", "c", "d"]


def epoch(text):
    return dt.datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()


T = epoch("2026-11-08T16:25:44.690Z")  # task 4473（e72b961d）を含む固定バイナリの update_installed
T2 = epoch("2026-11-09T01:50:37.191Z")  # task 4542（12d926e5）を含む固定バイナリの update_installed
E = epoch("2026-11-10T13:00:00Z")


def iso(t):
    return dt.datetime.fromtimestamp(t, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def load(data, kind):
    path = os.path.join(data, kind + ".jsonl")
    with open(path) as lines:
        events = [json.loads(line) for line in lines]
    for event in events:
        event["t"] = epoch(event["created_at"])
    return sorted(events, key=lambda e: e["id"])


def in_period(t):
    return T <= t < E


# ---- 大きさの標本（domain::disk::run_samples）と想定（expected_run_size）----


def reading(kind, payload):
    if kind == "worktree_removed" and payload.get("worktree_missing") is True:
        return "nothing", None
    unmeasured = payload.get("unmeasured")
    unmeasured = unmeasured is not None and (not isinstance(unmeasured, list) or len(unmeasured) > 0)
    bytes_ = payload.get("bytes")
    if isinstance(bytes_, int) and bytes_ >= 0 and not unmeasured:
        return "measured", bytes_
    return "unknown", None


class Samples:
    """時刻を進めながら run_samples を作る（未来を混ぜない）。"""

    def __init__(self, events):
        self.events = events
        self.i = 0
        self.runs = {}

    def advance(self, t):
        while self.i < len(self.events) and self.events[self.i]["t"] <= t:
            event = self.events[self.i]
            self.i += 1
            run_id = event.get("run_id")
            if run_id is None:
                continue
            run = self.runs.setdefault(run_id, {"build": None, "scratchpad": 0, "tmp": 0, "unknown": False, "at": None})
            run["at"] = event["t"] if run["at"] is None else max(run["at"], event["t"])
            state, bytes_ = reading(event["kind"], event["payload"])
            if state == "nothing":
                continue
            if state == "unknown":
                run["unknown"] = True
                continue
            part = PART[event["kind"]]
            if part == "build":
                run["build"] = bytes_ if run["build"] is None else max(run["build"], bytes_)
            else:
                run[part] = max(run[part], bytes_)

    def sample(self, run_id):
        run = self.runs.get(run_id)
        if run is None or run["unknown"] or run["build"] is None or run["at"] is None:
            return None
        return run["build"] + run["scratchpad"] + run["tmp"]

    def expected(self, t):
        """(想定, 窓の件数)。expected_run_estimate と同じ規則。"""
        recent = []
        for run in self.runs.values():
            if run["unknown"] or run["build"] is None or run["at"] is None:
                continue
            if t - run["at"] <= SAMPLE_WINDOW:
                recent.append((run["at"], run["build"] + run["scratchpad"] + run["tmp"]))
        recent.sort(key=lambda x: -x[0])
        sizes = sorted(b for _, b in recent[:SAMPLE_COUNT])
        n = len(sizes)
        if n == 0:
            return None, 0
        if n < QUANTILE_SAMPLES:
            return sizes[-1], n
        rank = -(-n * QUANTILE_TENTHS // 10)
        return sizes[max(rank, 1) - 1], n


def final_sizes(size_events):
    s = Samples(size_events)
    s.advance(float("inf"))
    return {run: s.sample(run) for run in s.runs}


# ---- run の期間と phase ----


def run_periods(data):
    """run_id → (claim, end)。end は claim の後の最後の session_closed か run_integrated（無ければ None）。"""
    claims = {}
    for e in load(data, "run_claimed"):
        claims.setdefault(e["run_id"], e["t"])
    ends = {}
    for kind in ["session_closed", "run_integrated"]:
        for e in load(data, kind):
            run = e.get("run_id")
            if run in claims and e["t"] >= claims[run]:
                ends[run] = max(ends.get(run, 0), e["t"])
    return {run: (c, ends.get(run)) for run, c in claims.items()}


class Phases:
    """run ごとの phase の列。phase_at(run, t) は t 以前の最後の phase。"""

    def __init__(self, events):
        self.by_run = {}
        for e in events:
            run = e.get("run_id")
            p = e["payload"]
            if run is None or "phase" not in p:
                continue
            self.by_run.setdefault(run, []).append((e["t"], p["phase"], p.get("holds")))
        for v in self.by_run.values():
            v.sort(key=lambda x: x[0])
        self.times = {r: [x[0] for x in v] for r, v in self.by_run.items()}

    def at(self, run, t):
        i = bisect.bisect_right(self.times[run], t) - 1
        return self.by_run[run][i] if i >= 0 else None

    def state(self, t):
        """(動いている run, 着地待ちの run, 着地の検証中の run)。"""
        moving, waiting, landing = [], [], []
        for run in self.by_run:
            now = self.at(run, t)
            if now is None:
                continue
            _, phase, holds = now
            if holds in ("worker_slot", "landing_slot") and phase not in NOT_MOVING:
                moving.append(run)
            if phase in LANDING_WAIT:
                waiting.append(run)
            if phase == "landing":
                landing.append(run)
        return moving, waiting, landing


def landing_spans(phases):
    """run ごとの着地の区間: phase landing の始まりから次の phase の始まりまで。

    landing の次に記録される phase は、終わり（push・needs_session など）か、復旧（cause run_recovered）で
    landing_queue に戻ったときで、どちらもその検証の終わりとする。復旧の後の検証は次の landing の区間に入る。
    """
    out = []
    for run, seq in phases.by_run.items():
        for i, (t, phase, _) in enumerate(seq):
            if phase == "landing":
                end = seq[i + 1][0] if i + 1 < len(seq) else None
                out.append((run, t, end))
    return sorted(out, key=lambda x: x[1])


# ---- hold_spans（domain::claim_hold::hold_spans の規則）----


def hold_spans(data, held, resumed):
    events = load(data, held) + load(data, resumed) + load(data, "supervisor_stopped")
    events.sort(key=lambda e: e["id"])
    spans, opened = [], None
    for event in events:
        kind = event["kind"]
        ends = kind in (held, resumed)
        if kind == "supervisor_stopped":
            ends = opened is not None and opened["payload"].get("supervisor") == event["payload"].get("supervisor")
        if not ends:
            continue
        if opened is not None:
            spans.append((opened, event["t"], kind))
            opened = None
        if kind == held:
            opened = event
    if opened is not None:
        spans.append((opened, E, "open"))
    return spans


def disk_intervals(spans):
    out = []
    for opened, end, _ in spans:
        if opened["payload"].get("reason") != "disk_space":
            continue
        a, b = max(opened["t"], T), min(end, E)
        if a < b:
            out.append((a, b))
    return merge(out)


def merge(intervals):
    merged = []
    for a, b in sorted(intervals):
        if merged and a <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], b)
        else:
            merged.append([a, b])
    return [tuple(x) for x in merged]


def total(intervals):
    return sum(b - a for a, b in intervals)


def clip(span, intervals):
    """span と intervals の重なりの長さ。"""
    a, b = span
    return sum(max(0.0, min(b, y) - max(a, x)) for x, y in intervals)


def covering(intervals, t):
    i = bisect.bisect_right(intervals, (t, float("inf"))) - 1
    return i >= 0 and intervals[i][0] <= t < intervals[i][1]


# ---- CSV ----


def read_samples(csv_dir):
    """[T, E) の sample を時刻順に。各行はその上の最後の header で読む。"""
    rows = []
    for path in sorted(glob.glob(os.path.join(csv_dir, "metrics-*.csv"))):
        day = os.path.basename(path)[8:16]
        if not "20261108" <= day <= "20261111":
            continue
        header = None
        with open(path, newline="") as f:
            for row in csv.reader(f):
                if row and row[0] == "time":
                    header = row
                    continue
                if header is None:
                    continue
                cells = dict(zip(header, row))
                try:
                    t = float(cells["unix"])
                except (KeyError, ValueError):
                    continue
                if not T <= t < E:
                    continue
                free = cells.get("disk_free_bytes") or None
                tot = cells.get("disk_total_bytes") or None
                rows.append({"t": t, "free": float(free) if free else None, "total": float(tot) if tot else None})
    rows.sort(key=lambda r: r["t"])
    return rows


def spans_of(samples):
    """sample ごとの代表の区間（次の sample まで。GAP を超えるか空きが空の行は欠測）。"""
    out = []
    for i, sample in enumerate(samples):
        nxt = samples[i + 1]["t"] if i + 1 < len(samples) else E
        ok = sample["free"] is not None and nxt - sample["t"] <= GAP
        out.append((sample["t"], nxt if ok else sample["t"]))
    return out


def missing_in(samples, intervals):
    """intervals の中で sample の代表の区間に入らない時間（欠測）。"""
    covered = merge([s for s in spans_of(samples) if s[1] > s[0]])
    return total(intervals) - sum(clip(iv, covered) for iv in intervals)


def total_at(samples, t):
    times = [s["t"] for s in samples]
    i = bisect.bisect_right(times, t) - 1
    while i >= 0 and samples[i]["total"] is None:
        i -= 1
    return samples[i]["total"] if i >= 0 else samples[0]["total"]


def free_series(samples):
    return [s["t"] for s in samples if s["free"] is not None], [s["free"] for s in samples if s["free"] is not None]


# ---- 今の大きさ（線形の仮定）----


def linear(run, t, periods, final, est):
    claim, end = periods.get(run, (None, None))
    size = final.get(run)
    if size is None:
        size = est or 0
    if claim is None:
        return size
    if end is None or end <= claim:
        return size if end is not None else size * 0.5
    return size * min(1.0, max(0.0, (t - claim) / (end - claim)))


# ---- Q1 ----


def q1(holds_events, ctx):
    rows = []
    for e in holds_events:
        if e["payload"].get("reason") != "disk_space" or not in_period(e["t"]):
            continue
        t = e["t"]
        ctx["samples"].advance(t)
        est, n = ctx["samples"].expected(t)
        tot = total_at(ctx["csv"], t)
        reserve = max(MIN_RESERVE, math.ceil(tot * RESERVE_SHARE))
        moving, waiting, landing = ctx["phases"].state(t)
        thr = e["payload"]["threshold"]
        lw = bool(waiting)
        # phase から読んだ候補。T2 より前は残りが 0（決定7）なので、閾値が想定の何本分かで実際に足した項が決まる。
        candidate = bool(moving and est is not None and lw)
        if t < T2 and moving and est:
            added = round((thr - reserve) / est) >= 2
        else:
            added = candidate
        add_landing = est if added else 0
        if moving and est is not None:
            remaining = thr - reserve - est - add_landing
        else:
            remaining = thr - reserve
        rows.append(
            {
                "at": e["created_at"],
                "before_t2": t < T2,
                "free_gib": e["payload"]["value"] / GIB,
                "threshold_gib": thr / GIB,
                "reserve_gib": reserve / GIB,
                "expected_gib": (est or 0) / GIB,
                "samples": n,
                "moving": len(moving),
                "landing_in_progress": len(landing),
                "landing_waiting_runs": len(waiting),
                "landing_candidate": candidate,
                "landing_added_from": "record" if t < T2 else "phase",
                "landing_added_gib": add_landing / GIB,
                "remaining_gib": remaining / GIB,
                # 着地待ちの項を足したか: 閾値から最低ラインと想定を引いた残りが想定の何本分か
                "multiples_of_expected": (thr - reserve) / est if est else None,
            }
        )
    return rows


# ---- Q2 ----


BUILD_PHASES = {"worker", "resume", "revise", "recovery", "landing", "e2e", "verifying"}


def q2(ctx, lspans, cleanups, vcmds, disk_iv):
    """着地の検証ごとに、検証の有無・その前の掃除・検証の後の最初の掃除の大きさを対応づける。

    cleanups: run → [(時刻, kind, bytes か None)]（build_outputs_removed と worktree_removed、時刻順）。
    検証の後の掃除は、着地の区間の終わりから次の build する phase の始まりまでの最初のものだけをその検証に帰属させ、
    それが無いか bytes が測れていなければ不明とする。
    """
    times, frees = ctx["times"], ctx["frees"]
    phases = ctx["phases"]
    rows = []
    for run, t0, t1 in lspans:
        if not in_period(t0) or t1 is None:
            continue
        ctx["samples"].advance(t0)
        est, _ = ctx["samples"].expected(t0)
        seq = phases.by_run[run]
        # 着地の前の最後の build する phase の終わり（無ければ run の claim）
        prev_end = ctx["periods"].get(run, (t0, None))[0]
        for k, (pt, phase, _) in enumerate(seq):
            if pt >= t0:
                break
            if phase in BUILD_PHASES and k + 1 < len(seq):
                prev_end = seq[k + 1][0]
        own = cleanups.get(run, [])
        removed_before = any(prev_end <= ct < t0 and kind == "build_outputs_removed" for ct, kind, _ in own)
        # 区間の終わりと同じ時刻に始まる次の landing も次の build に数える
        next_build = min((pt for pt, phase, _ in seq if pt >= t1 and phase in BUILD_PHASES), default=float("inf"))
        after = [(ct, kind, b) for ct, kind, b in own if t1 - 1 <= ct < next_build]
        attributed = after[0][2] if after else None
        verified = any(t0 <= vt <= t1 for vt in vcmds.get(run, []))
        i = bisect.bisect_right(times, t0) - 1
        j = bisect.bisect_right(times, t1)
        drop = (frees[i] - min(frees[i:j])) if i >= 0 and j > i + 1 else None
        rows.append(
            {
                "run": run,
                "start": iso(t0),
                "minutes": (t1 - t0) / 60,
                "verified": verified,
                "expected_gib": (est or 0) / GIB,
                "drop_gib": drop / GIB if drop is not None else None,
                "build_removed_before": removed_before,
                "cleanup_after_gib": attributed / GIB if attributed is not None else None,
                "cleanup_after_kind": after[0][1] if after else None,
                "in_disk_hold": covering(disk_iv, t0),
            }
        )
    return rows


def summarize_q2(rows):
    def ratios(sel, key):
        return [r[key] / r["expected_gib"] for r in sel if r[key] is not None and r["expected_gib"]]

    def part(sel):
        if not sel:
            return {"n": 0}
        drop = ratios(sel, "drop_gib")
        size = ratios(sel, "cleanup_after_gib")
        known = [r["cleanup_after_gib"] for r in sel if r["cleanup_after_gib"] is not None]
        return {
            "n": len(sel),
            "expected_median_gib": statistics.median(r["expected_gib"] for r in sel),
            "drop_n": len(drop),
            "drop_median_gib": statistics.median([r["drop_gib"] for r in sel if r["drop_gib"] is not None] or [0]),
            "drop_over_expected_median": statistics.median(drop) if drop else None,
            "drop_over_expected_max": max(drop) if drop else None,
            "drop_at_least_expected": sum(x >= 1 for x in drop),
            "size_n": len(size),
            "size_unknown": len(sel) - len(known),
            "size_median_gib": statistics.median(known) if known else None,
            "size_over_expected_median": statistics.median(size) if size else None,
            "size_over_expected_max": max(size) if size else None,
            "size_at_least_expected": sum(x >= 1 for x in size),
        }

    verified = [r for r in rows if r["verified"]]
    return {
        "landings": len(rows),
        "verified": len(verified),
        "unverified": len(rows) - len(verified),
        "unverified_short": sum(r["minutes"] < 2 for r in rows if not r["verified"]),
        "verified_short": sum(r["minutes"] < 2 for r in verified),
        "all": part(verified),
        "build_removed_before": part([r for r in verified if r["build_removed_before"]]),
        "build_kept": part([r for r in verified if not r["build_removed_before"]]),
    }


# ---- Q3 と Q4 ----


def needs(way, reserve, est, moving_n, remaining, waiting, landing, sizes_now):
    """(claim の必要量, claims_wait)。DiskState::needs の claim と、着地待ちの項の足し方 a〜d。"""
    if moving_n == 0:
        return reserve, bool(waiting)
    if est is None:
        return reserve, True
    if way == "a":
        add = est if waiting else 0
    elif way == "b":
        add = est if waiting and len(landing) < LANDING_SLOTS else 0
    elif way == "c":
        add = max((max(0.0, est - sizes_now[r]) for r in waiting), default=0.0)
    else:
        add = 0
    return reserve + est + remaining + add, False


def simulate(way, ctx, pre_t2_zero=False):
    """足し方 way で sample の順に流す。pre_t2_zero は参考の感度: T2 より前の残りを 0 と数える（その頃の固定バイナリ）。"""
    csv_rows, spans, disk_iv = ctx["csv"], ctx["spans"], ctx["disk_iv"]
    samples = Samples(ctx["size_events"])
    phases, periods, final = ctx["phases"], ctx["periods"], ctx["final"]
    removed = ctx["removed_before"]
    durations = ctx["durations"]  # (end, duration) の end 順
    ends = [d[0] for d in durations]
    virtual = []  # [start, duration, size]
    hold, claimable, virtual_n, short = 0.0, 0, 0, []
    overlap = {"samples": 0, "secs": 0.0, "secs_landing_added": 0.0}
    added_secs = 0.0
    decisive = 0.0
    for idx, row in enumerate(csv_rows):
        t = row["t"]
        samples.advance(t)
        virtual = [v for v in virtual if v[0] + v[1] > t]
        if row["free"] is None:
            continue
        span = spans[idx]
        dur = span[1] - span[0]
        if dur <= 0:
            # 欠測の標本: 控えにも容量不足にも数えず、仮想の run も足さない（間の欠測で容量不足の区間は分かれる）
            continue
        in_record = clip(span, disk_iv)
        est, _ = samples.expected(t)
        tot = row["total"] or total_at(csv_rows, t)
        reserve = max(MIN_RESERVE, math.ceil(tot * RESERVE_SHARE))
        moving, waiting, landing = phases.state(t)
        v_sizes = [v[2] * min(1.0, (t - v[0]) / v[1]) for v in virtual]
        free_v = row["free"] - sum(v_sizes)
        sizes_now = {}
        for r in set(moving) | set(waiting):
            if any(x < t for x in removed.get(r, [])) and r in waiting:
                sizes_now[r] = 0.0
            else:
                sizes_now[r] = linear(r, t, periods, final, est)
        remaining = 0.0
        if est is not None and not (pre_t2_zero and t < T2):
            remaining = sum(max(0.0, est - sizes_now[r]) for r in moving)
            remaining += sum(max(0.0, est - s) for s in v_sizes)
        moving_n = len(moving) + len(virtual)
        claim, wait = needs(way, reserve, est, moving_n, remaining, waiting, landing, sizes_now)
        held_here = (free_v < claim) and not wait
        go = not held_here and not wait
        in_hold = covering(disk_iv, t)
        if way == "a" and in_record and held_here:
            # 着地待ちの項だけが控えを決めた時間: 項を外すと通る（仮想の run の後の影響は含まない）
            claim_d, _ = needs("d", reserve, est, moving_n, remaining, waiting, landing, sizes_now)
            if free_v >= claim_d:
                decisive += in_record
        if way == "a" and moving_n and est is not None and waiting:
            added_secs += dur
            if landing:
                overlap["secs_landing_added"] += dur
        if held_here:
            hold += in_record
        if in_hold:
            if go and est is not None and moving_n < PARALLEL:
                lo = bisect.bisect_left(ends, t - 14 * 86400)
                hi = bisect.bisect_left(ends, t)
                recent = [d for _, d in durations[lo:hi]]
                length = statistics.median(recent) if recent else 3600.0
                virtual.append([t, length, est])
                virtual_n += 1
        if free_v <= 0:
            short.append(idx)
    regions, prev = 0, None
    for idx in short:
        if prev is None or idx != prev + 1 or csv_rows[idx]["t"] - csv_rows[prev]["t"] > GAP:
            regions += 1
        prev = idx
    claims = ctx["period_claims"] + virtual_n
    out = {
        "way": way,
        "pre_t2_remaining_zero": pre_t2_zero,
        "hold_h": hold / 3600,
        "short_regions": regions,
        "short_samples": len(short),
        "virtual_runs": virtual_n,
        "claims": claims,
        "ratio": regions / claims if claims else None,
    }
    if way == "a":
        out["landing_added_h"] = added_secs / 3600
        out["landing_added_while_landing_h"] = overlap["secs_landing_added"] / 3600
        out["landing_term_decisive_in_hold_h"] = decisive / 3600
    return out


def q3(ctx, q1_rows):
    """着地の検証中の run が残りとして数えられ、同時に着地待ちの想定も足された時間と件数。"""
    csv_rows, spans = ctx["csv"], ctx["spans"]
    phases = ctx["phases"]
    secs = {"landing_added": 0.0, "landing_added_and_landing_moving": 0.0, "waiting_2plus": 0.0, "period": 0.0}
    for idx, row in enumerate(csv_rows):
        if row["free"] is None:
            continue
        dur = spans[idx][1] - spans[idx][0]
        moving, waiting, landing = phases.state(row["t"])
        secs["period"] += dur
        if moving and waiting:
            secs["landing_added"] += dur
            if landing:
                secs["landing_added_and_landing_moving"] += dur
            if len(waiting) >= 2:
                secs["waiting_2plus"] += dur
    holds_added = [r for r in q1_rows if r["landing_added_gib"] > 0]
    return {
        "hours": {k: v / 3600 for k, v in secs.items()},
        "holds": len(q1_rows),
        "holds_landing_added": len(holds_added),
        "holds_landing_added_and_landing_moving": sum(r["landing_in_progress"] > 0 for r in holds_added),
        "holds_waiting_runs_max": max((r["landing_waiting_runs"] for r in q1_rows), default=0),
    }


def main():
    data, csv_dir, out_path = sys.argv[1:4]
    size_events = sorted((e for k in SAMPLE_KINDS for e in load(data, k)), key=lambda e: (e["t"], e["id"]))
    periods = run_periods(data)
    phases = Phases(load(data, "run_phase_changed"))
    csv_rows = read_samples(csv_dir)
    times, frees = free_series(csv_rows)
    spans = spans_of(csv_rows)
    held = hold_spans(data, "claim_held", "claim_resumed")
    disk_iv = disk_intervals(held)
    removed_before = {}
    for e in load(data, "build_outputs_removed"):
        if e.get("run_id"):
            removed_before.setdefault(e["run_id"], []).append(e["t"])
    cleanups = {}
    for e in sorted(load(data, "build_outputs_removed") + load(data, "worktree_removed"), key=lambda e: (e["t"], e["id"])):
        state, b = reading(e["kind"], e["payload"])
        if e.get("run_id") and state != "nothing":
            cleanups.setdefault(e["run_id"], []).append((e["t"], e["kind"], b if state == "measured" else None))
    vcmds = {}
    for e in load(data, "verification_command"):
        if e.get("run_id"):
            vcmds.setdefault(e["run_id"], []).append(e["t"])
    durations = sorted((end, end - claim) for claim, end in periods.values() if end is not None and end > claim)
    ctx = {
        "samples": Samples(size_events),
        "size_events": size_events,
        "periods": periods,
        "phases": phases,
        "csv": csv_rows,
        "times": times,
        "frees": frees,
        "spans": spans,
        "disk_iv": disk_iv,
        "final": final_sizes(size_events),
        "removed_before": removed_before,
        "durations": durations,
        "period_claims": sum(1 for c, _ in periods.values() if in_period(c)),
    }
    q1_rows = q1(load(data, "claim_held"), ctx)
    ctx["samples"] = Samples(size_events)
    q2_rows = q2(ctx, landing_spans(phases), cleanups, vcmds, disk_iv)
    q4 = [simulate(w, ctx) for w in WAYS]
    a, d = q4[0]["hold_h"], q4[3]["hold_h"]
    q4_zero = [simulate(w, ctx, pre_t2_zero=True) for w in WAYS]
    recorded_h = total(disk_iv) / 3600
    missing_h = missing_in(csv_rows, disk_iv) / 3600
    disk_full = sum(
        1
        for e in load(data, "verification_command")
        if in_period(e["t"]) and ((e["payload"].get("failure") or {}).get("class") == "disk_full")
    )
    full_resume = sum(
        1 for e in load(data, "auto_repaired") if in_period(e["t"]) and e["payload"].get("repair") == "full_disk_resume"
    )
    period_missing_h = missing_in(csv_rows, [(T, E)]) / 3600
    summary = {
        "period": {"T": iso(T), "T2": iso(T2), "E": iso(E), "hours": (E - T) / 3600},
        "csv_rows": len(csv_rows),
        "period_claims": ctx["period_claims"],
        "recorded_disk_hold_h": recorded_h,
        "recorded_disk_hold_spans": len(disk_iv),
        "recorded_disk_hold_missing_h": missing_h,
        "recorded_disk_hold_missing_share": missing_h / recorded_h if recorded_h else None,
        "period_missing_h": period_missing_h,
        "observed_failures": {"disk_full_verification": disk_full, "full_disk_resume": full_resume},
        "q1": q1_rows,
        "q1_summary": {
            "holds": len(q1_rows),
            "before_t2": sum(r["before_t2"] for r in q1_rows),
            "landing_added": sum(r["landing_added_gib"] > 0 for r in q1_rows),
            "landing_added_share_of_threshold_median": statistics.median(
                [r["landing_added_gib"] / r["threshold_gib"] for r in q1_rows if r["landing_added_gib"] > 0] or [0]
            ),
            "residual_negative": sum(r["remaining_gib"] < -0.5 for r in q1_rows),
            "before_t2_phase_matches_record": sum(
                r["landing_candidate"] == (r["landing_added_gib"] > 0) for r in q1_rows if r["before_t2"]
            ),
        },
        "q2": q2_rows,
        "q2_summary": summarize_q2(q2_rows),
        "q3": q3(ctx, q1_rows),
        "q4": q4,
        "q4_sensitivity_pre_t2_remaining_zero": q4_zero,
        "q4_sensitivity_F": (q4_zero[0]["hold_h"] - q4_zero[3]["hold_h"]) / q4_zero[0]["hold_h"] if q4_zero[0]["hold_h"] else None,
        "a_minus_recorded_h": a - recorded_h,
        "a_vs_recorded_share": (a - recorded_h) / recorded_h if recorded_h else None,
        "F": (a - d) / a if a else None,
        "R": {w["way"]: w["ratio"] for w in q4[1:]},
    }
    with open(out_path, "w") as f:
        json.dump(summary, f, indent=1, ensure_ascii=False, sort_keys=True)
        f.write("\n")


if __name__ == "__main__":
    main()
