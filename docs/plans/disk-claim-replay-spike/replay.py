#!/usr/bin/env python3
"""空き容量の claim の規則の当て直し（docs/plans/disk-claim-replay-spike.md）。

使い方: replay.py DATA_DIR CSV_DIR OUT_JSON
DATA_DIR は collect.sh の JSONL、CSV_DIR は host/metrics-YYYYMMDD.csv の dir。どちらも読むだけ。
同じ入力に決まる計算で、乱数も時刻も使わない。仮定は文書の「仮定」の節に書く。
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
SIZE_KINDS = ["build_outputs_removed", "scratchpad_removed", "run_tmp_removed"]
GAP = 300  # 秒。隣の sample との間がこれを超えれば欠測
PARALLEL = 3  # dagq.toml の [supervisor] parallel
CLAIM_FACTOR, INTEGRATE_FACTOR, SAMPLE_RUNS = 2.0, 1.5, 20  # domain::disk の既定（dagq.toml に [disk] は無い）
FEW = 5  # 記録がこれ未満なら最大


def epoch(text):
    return dt.datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()


PERIOD_START = epoch("2026-10-11T00:00:00Z")
PERIOD_END = epoch("2026-11-04T12:00:00Z")
SPLIT_224 = epoch("2026-10-28T00:00:00Z")  # goal 224 の close の日


def load(data, kind):
    with open(os.path.join(data, kind + ".jsonl")) as lines:
        events = [json.loads(line) for line in lines]
    for event in events:
        event["t"] = epoch(event["created_at"])
    return sorted(events, key=lambda e: e["id"])


def in_period(t):
    return PERIOD_START <= t < PERIOD_END


def quantile(values, q):
    """最近順位法: 昇順の ceil(q × n) 番目。q = 1.0 は最大。"""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(q * len(ordered)) - 1)]


class Sizes:
    """時刻を進めながら run の大きさを作る（未来を混ぜない）。

    run の大きさは種類ごとに run の中の最大を取って足す。stream は最後の記録の順の run の列。
    """

    def __init__(self, events, population):
        self.events = events
        self.population = population  # "build": build の記録のある run だけ、"any": どれかの記録のある run
        self.i = 0
        self.per_run = {}
        self.last = {}
        self.order = []

    def advance(self, t):
        changed = False
        while self.i < len(self.events) and self.events[self.i]["t"] <= t:
            event = self.events[self.i]
            self.i += 1
            run, bytes_ = event["run_id"], event["payload"].get("bytes")
            if run is None or bytes_ is None:
                continue
            kinds = self.per_run.setdefault(run, {})
            kinds[event["kind"]] = max(kinds.get(event["kind"], 0), bytes_)
            if self.population == "build" and "build_outputs_removed" not in kinds:
                continue
            if run in self.last:
                self.order.remove(run)
            self.order.append(run)
            self.last[run] = event["t"]
            changed = True
        return changed

    def window(self, t, count, days):
        chosen = []
        for run in reversed(self.order[-count:]):
            if t - self.last[run] <= days * 86400:
                chosen.append(run)
        return chosen

    def estimate(self, t, q, count, days):
        """想定の bytes。記録が無ければ None、FEW 未満なら最大。"""
        chosen = self.window(t, count, days)
        if not chosen:
            return None
        sizes = [sum(self.per_run[run].values()) for run in chosen]
        return max(sizes) if len(sizes) < FEW else quantile(sizes, q)


def run_periods(data):
    """run_id → (claim, end, task)。end は最後の session_closed か run_integrated の遅い方。"""
    claims = {e["run_id"]: e for e in load(data, "run_claimed")}
    ends = {}
    for kind in ["session_closed", "run_integrated"]:
        for event in load(data, kind):
            run = event["run_id"]
            if run in claims:
                ends[run] = max(ends.get(run, 0), event["t"])
    return {
        run: (e["t"], max(ends.get(run, e["t"]), e["t"]), e["task_id"]) for run, e in claims.items()
    }


def occupancy(data, periods):
    """感度の表の「動いている run」: session が開いているか着地の検証のコマンドが走っている区間。

    session の開始は opened_event_id の時刻で、読んだ event の id と時刻の対応から引く（直前の読んだ event の時刻）。
    run_id → (動いている区間の列, build する session の区間の列)。build するのは worker・resume・revise と検証。
    """
    ids = []
    for kind in SIZE_KINDS + ["run_claimed", "session_closed", "run_integrated", "claim_held", "claim_resumed", "auto_repaired", "verification_command"]:
        ids += [(e["id"], e["t"]) for e in load(data, kind)]
    ids.sort()
    keys = [i for i, _ in ids]
    busy, building = {}, {}
    for event in load(data, "session_closed"):
        run = event["run_id"]
        if run not in periods:
            continue
        opened = event["payload"].get("opened_event_id")
        at = bisect.bisect_right(keys, opened) - 1 if opened is not None else -1
        start = max(ids[at][1], periods[run][0]) if at >= 0 else periods[run][0]
        if start >= event["t"]:
            continue
        busy.setdefault(run, []).append((start, event["t"]))
        if event["payload"].get("kind") in ("worker", "resume", "revise"):
            building.setdefault(run, []).append((start, event["t"]))
    for event in load(data, "verification_command"):
        run = event["run_id"]
        duration = event["payload"].get("duration_secs")
        if run not in periods or not duration:
            continue
        busy.setdefault(run, []).append((event["t"] - duration, event["t"]))
        building.setdefault(run, []).append((event["t"] - duration, event["t"]))
    return {r: merge(v) for r, v in busy.items()}, {r: merge(v) for r, v in building.items()}


def per_sample(samples, per_run, periods):
    """sample ごとに、その時刻に区間のある run の claim の時刻の列を作る。"""
    times = [s["t"] for s in samples]
    out = [[] for _ in samples]
    for run, intervals in per_run.items():
        for a, b in intervals:
            lo, hi = bisect.bisect_left(times, a), bisect.bisect_left(times, b)
            for i in range(lo, hi):
                out[i].append(periods[run][0])
    return out


def hold_spans(data, held, resumed):
    """domain::claim_hold::hold_spans の規則で控えの区間を作る。読むのは全 reason。"""
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
        spans.append((opened, PERIOD_END, "open"))
    return spans


def disk_intervals(spans):
    out = []
    for opened, end, _ in spans:
        if opened["payload"].get("reason") != "disk_space":
            continue
        a, b = max(opened["t"], PERIOD_START), min(end, PERIOD_END)
        if a < b:
            out.append((a, b))
    return out


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


def overlap(xs, ys):
    i = j = 0
    sum_ = 0.0
    while i < len(xs) and j < len(ys):
        a, b = max(xs[i][0], ys[j][0]), min(xs[i][1], ys[j][1])
        if a < b:
            sum_ += b - a
        if xs[i][1] < ys[j][1]:
            i += 1
        else:
            j += 1
    return sum_


def covering(intervals, t):
    i = bisect.bisect_right(intervals, (t, float("inf"))) - 1
    return i >= 0 and intervals[i][0] <= t < intervals[i][1]


def read_samples(csv_dir):
    """期間の sample を時刻順に。各行はその上の最後の header で読む（列が増えた日）。"""
    rows = []
    for path in sorted(glob.glob(os.path.join(csv_dir, "metrics-*.csv"))):
        day = os.path.basename(path)[8:16]
        if not "20261010" <= day <= "20261105":
            continue
        header = None
        with open(path, newline="") as f:
            for row in csv.reader(f):
                if row and row[0] == "time":
                    header = row
                    continue
                cells = dict(zip(header, row))
                try:
                    t = float(cells["unix"])
                except (KeyError, ValueError):
                    continue
                if not PERIOD_START <= t < PERIOD_END:
                    continue
                free = cells.get("disk_free_bytes") or None
                rows.append(
                    {
                        "t": t,
                        "free": float(free) if free else None,
                        "total": float(cells["disk_total_bytes"]) if cells.get("disk_total_bytes") else None,
                        "has_landing": "landing_runs" in header,
                        "landing": cells.get("landing_runs") or None,
                    }
                )
    rows.sort(key=lambda r: r["t"])
    return rows


def spans_of(samples):
    """sample ごとの代表の区間（次の sample まで。GAP を超えれば GAP の外は欠測）。"""
    out = []
    for i, sample in enumerate(samples):
        nxt = samples[i + 1]["t"] if i + 1 < len(samples) else PERIOD_END
        out.append((sample["t"], nxt if nxt - sample["t"] <= GAP else sample["t"]))
    return out


def landing_waiting(samples):
    """sample ごとの着地待ちの想定。値のある行はその値、空の行は 30 分以内の直前の値、それも無ければ 1 本。"""
    out, last, last_t = [], None, None
    for sample in samples:
        value = sample["landing"]
        if value is not None:
            last, last_t = int(value), sample["t"]
            out.append(last >= 1)
        elif sample["has_landing"] and last is not None and sample["t"] - last_t <= 1800:
            out.append(last >= 1)
        else:
            out.append(True)
    return out


def table_a(data_events, periods, population):
    """claim の時点の想定と実際の大きさ。実際は期間の後の記録まで含めた run の大きさ。"""
    final = {}
    for event in data_events:
        run, bytes_ = event["run_id"], event["payload"].get("bytes")
        if run is None or bytes_ is None:
            continue
        kinds = final.setdefault(run, {})
        kinds[event["kind"]] = max(kinds.get(event["kind"], 0), bytes_)
    actual = {
        run: sum(kinds.values())
        for run, kinds in final.items()
        if population == "any" or "build_outputs_removed" in kinds
    }
    claims = sorted((p[0], run) for run, p in periods.items() if in_period(p[0]))
    rows = []
    for qname, q in [("p90", 0.90), ("p95", 0.95), ("最大", 1.0)]:
        for count in [20, 50]:
            for days in [7, 14, 30]:
                sizes = Sizes(data_events, population)
                pairs = []
                for t, run in claims:
                    sizes.advance(t)
                    if run not in actual:
                        continue
                    est = sizes.estimate(t, q, count, days)
                    if est is not None:
                        pairs.append((t, actual[run], est))
                rows.append(
                    {
                        "q": qname,
                        "count": count,
                        "days": days,
                        "all": over_row(pairs),
                        "before": over_row([p for p in pairs if p[0] < SPLIT_224]),
                        "after": over_row([p for p in pairs if p[0] >= SPLIT_224]),
                    }
                )
    return rows


def over_row(pairs):
    over = [a - e for _, a, e in pairs if a > e]
    return {
        "runs": len(pairs),
        "over_rate": len(over) / len(pairs) if pairs else None,
        "over_median_gib": statistics.median(over) / GIB if over else None,
        "over_max_gib": max(over) / GIB if over else None,
        "est_median_gib": statistics.median(e for _, _, e in pairs) / GIB if pairs else None,
    }


def current_rule(samples, spans, size_events, waiting):
    """今の規則の sample ごとの判定（直近 SAMPLE_RUNS 件の種類ごとの最大の和 × 係数）。"""
    per_kind = {k: [] for k in SIZE_KINDS}
    i = 0
    claim, landing, needs = [], [], []
    for idx, sample in enumerate(samples):
        while i < len(size_events) and size_events[i]["t"] <= sample["t"]:
            bytes_ = size_events[i]["payload"].get("bytes")
            if bytes_ is not None:
                per_kind[size_events[i]["kind"]].append(bytes_)
            i += 1
        size = sum(max(v[-SAMPLE_RUNS:]) for v in per_kind.values() if v)
        needs.append(size)
        if sample["free"] is None:
            continue
        if sample["free"] < math.ceil(size * CLAIM_FACTOR):
            claim.append(spans[idx])
        if waiting[idx] and sample["free"] < math.ceil(size * INTEGRATE_FACTOR):
            landing.append(spans[idx])
    return merge(claim), merge(landing), needs


def threshold_check(claim_spans, size_events):
    """記録の閾値と、同じ時刻に再現した閾値の比（再現の式の確かめ）。"""
    per_kind = {k: [] for k in SIZE_KINDS}
    i = 0
    ratios = []
    for opened, _, _ in claim_spans:
        if opened["payload"].get("reason") != "disk_space" or not in_period(opened["t"]):
            continue
        while i < len(size_events) and size_events[i]["t"] <= opened["t"]:
            bytes_ = size_events[i]["payload"].get("bytes")
            if bytes_ is not None:
                per_kind[size_events[i]["kind"]].append(bytes_)
            i += 1
        size = sum(max(v[-SAMPLE_RUNS:]) for v in per_kind.values() if v)
        ratios.append(size * CLAIM_FACTOR / opened["payload"]["threshold"])
    near = sum(0.95 <= r <= 1.05 for r in ratios)
    return {"holds": len(ratios), "within_5pct": near, "median_ratio": statistics.median(ratios)}


def simulate(config, ctx):
    """案の規則を sample の順に流す。

    config: reserve_pct・assume（"zero" か "linear"）・q・count・days・population と、感度の表の
    running（"period": 説明の (1) の run の期間、"busy": 動いている run）・
    remaining（"all": 走っている run すべて、"building": build する session のある run だけ、"none": 足さない）・
    landing_need（claim の判断に着地待ちの検証の想定を足すか）。
    """
    samples, spans, waiting = ctx["samples"], ctx["spans"], ctx["waiting"]
    periods = ctx["periods"]
    sizes = Sizes(ctx["size_events"], config.get("population", "any"))
    q, count, days = config.get("q", 0.90), config.get("count", 20), config.get("days", 14)
    reserve_pct, assume = config["reserve_pct"], config["assume"]
    running_at = ctx[config.get("running", "period")]
    remaining_mode = config.get("remaining", "all")
    landing_need_on = config.get("landing_need", True)
    virtual = []  # (start, end, size)
    claim_hold, landing_hold, virtual_claims = [], [], 0
    needs_seen, nov3_virtual = [], 0
    short_samples = []
    last_total = None
    for idx, sample in enumerate(samples):
        t = sample["t"]
        sizes.advance(t)
        virtual = [v for v in virtual if v[1] > t]
        free = sample["free"]
        last_total = sample["total"] or last_total
        if free is None or last_total is None:
            continue
        reserve = max(5 * GIB, reserve_pct * last_total)
        free_v = free - sum(v[2] for v in virtual)
        est = sizes.estimate(t, q, count, days)
        running = list(running_at[idx]) + [v[0] for v in virtual]
        if remaining_mode == "all":
            counted = running
        elif remaining_mode == "building":
            counted = list(ctx["building"][idx]) + [v[0] for v in virtual]
        else:
            counted = []
        window = sizes.window(t, count, days)
        finished = [periods[r][1] - periods[r][0] for r in window if r in periods and periods[r][0] < periods[r][1] <= t]
        duration = statistics.median(finished) if finished else ctx["median_duration"]
        forward = not running and free_v >= reserve
        if est is None:
            claim_ok = landing_ok = forward
        else:
            remaining = 0.0
            for start in counted:
                if assume == "zero":
                    remaining += est
                else:
                    remaining += max(0.0, est * (1 - min(1.0, (t - start) / duration)))
            landing_need = est if waiting[idx] and landing_need_on else 0
            claim_ok = forward or free_v - reserve >= est + remaining + landing_need
            needs_seen.append(reserve + est + remaining + landing_need)
            landing_ok = forward or free_v - reserve >= est
        if not claim_ok:
            claim_hold.append(spans[idx])
        if waiting[idx] and not landing_ok:
            landing_hold.append(spans[idx])
        if claim_ok and est is not None and covering(ctx["claim_union"], t) and len(running) < PARALLEL:
            virtual.append((t, t + duration, est))
            virtual_claims += 1
            nov3_virtual += NOV3[0] <= t < NOV3[1]
            free_v -= est
        # 容量不足はこの sample で足した仮想の run を含む空きで数える
        if free_v <= 0:
            short_samples.append(idx)
    claim_merged = merge(claim_hold)
    landing_merged = merge(landing_hold)
    union = merge(claim_hold + landing_hold)
    longest = max((b - a for a, b in union), default=0.0)
    # 容量不足の区間: 連続する sample が 1 区間。GAP を超えれば別の区間。
    regions = 0
    prev = None
    for idx in short_samples:
        if prev is None or idx != prev + 1 or samples[idx]["t"] - samples[prev]["t"] > GAP:
            regions += 1
        prev = idx
    claims = ctx["period_claims"] + virtual_claims
    return {
        "claim_h": total(claim_merged) / 3600,
        "landing_h": total(landing_merged) / 3600,
        "union_h": total(union) / 3600,
        "longest_h": longest / 3600,
        "union": union,
        "short_regions": regions,
        "short_samples": len(short_samples),
        "virtual_claims": virtual_claims,
        "nov3_virtual": nov3_virtual,
        "need_median_gib": statistics.median(needs_seen) / GIB,
        "claims": claims,
        "ratio": regions / claims,
    }


def strip(result):
    return {k: v for k, v in result.items() if k != "union"}


NOV3 = (epoch("2026-11-03T23:25:31Z"), epoch("2026-11-04T11:50:00Z"))

VARIANTS = [
    ("S1 動いている run だけ数える・今の大きさ 0", {"running": "busy", "assume": "zero"}),
    ("S2 動いている run だけ数える・線形", {"running": "busy", "assume": "linear"}),
    ("S3 build する session の run だけ残りを足す・線形", {"running": "busy", "remaining": "building", "assume": "linear"}),
    ("S4 S3 から着地待ちの想定を除く", {"running": "busy", "remaining": "building", "assume": "linear", "landing_need": False}),
    ("S5 新しい run の想定だけ（残りも着地も足さない）", {"running": "busy", "remaining": "none", "assume": "linear", "landing_need": False}),
    ("S6 S5 を p95", {"running": "busy", "remaining": "none", "assume": "linear", "landing_need": False, "q": 0.95}),
    ("S7 S5 を reserve 10%", {"running": "busy", "remaining": "none", "assume": "linear", "landing_need": False, "reserve_pct": 0.10}),
    ("S8 S4 を reserve 10%", {"running": "busy", "remaining": "building", "assume": "linear", "landing_need": False, "reserve_pct": 0.10}),
    ("S9 S3 を reserve 5 GiB", {"running": "busy", "remaining": "building", "assume": "linear", "reserve_pct": 0.0}),
    ("S10 S4 を reserve 5 GiB", {"running": "busy", "remaining": "building", "assume": "linear", "landing_need": False, "reserve_pct": 0.0}),
    ("S11 S5 を reserve 5 GiB", {"running": "busy", "remaining": "none", "assume": "linear", "landing_need": False, "reserve_pct": 0.0}),
]


def main():
    data, csv_dir, out_path = sys.argv[1:4]
    size_events = sorted(
        [e for kind in SIZE_KINDS for e in load(data, kind)], key=lambda e: e["t"]
    )
    periods = run_periods(data)
    claim_spans = hold_spans(data, "claim_held", "claim_resumed")
    landing_spans = hold_spans(data, "landing_held", "landing_resumed")
    claim_rec = merge(disk_intervals(claim_spans))
    landing_rec = merge(disk_intervals(landing_spans))
    union_rec = merge(claim_rec + landing_rec)

    samples = read_samples(csv_dir)
    spans = spans_of(samples)
    waiting = landing_waiting(samples)
    gaps = [(samples[i]["t"], samples[i + 1]["t"]) for i in range(len(samples) - 1) if samples[i + 1]["t"] - samples[i]["t"] > GAP]
    head_gap = samples[0]["t"] - PERIOD_START
    tail_gap = PERIOD_END - samples[-1]["t"]
    period_secs = PERIOD_END - PERIOD_START
    gap_secs = total(gaps) + max(0, head_gap) + max(0, tail_gap)
    covered = merge(spans)

    repro_claim, repro_landing, needs = current_rule(samples, spans, size_events, waiting)
    repro_union = merge(repro_claim + repro_landing)
    rec_in_csv = overlap(union_rec, covered)
    repro_overlap = overlap(union_rec, repro_union)
    observed_short = [s for s in samples if s["free"] is not None and s["free"] <= 0]

    durations = [p[1] - p[0] for p in periods.values() if in_period(p[0]) and p[1] > p[0]]
    median_duration = statistics.median(durations)
    period_claims = sum(1 for p in periods.values() if in_period(p[0]))
    busy, building = occupancy(data, periods)
    ctx = {
        "busy": per_sample(samples, busy, periods),
        "building": per_sample(samples, building, periods),
        "period": per_sample(samples, {r: [(p[0], p[1])] for r, p in periods.items() if p[1] > p[0]}, periods),
        "samples": samples,
        "spans": spans,
        "waiting": waiting,
        "periods": periods,
        "size_events": size_events,
        "claim_union": claim_rec,
        "median_duration": median_duration,
        "period_claims": period_claims,
    }

    def table_b_rows(population):
        rows = []
        for pct in [0.05, 0.10]:
            for assume in ["zero", "linear"]:
                result = simulate({"reserve_pct": pct, "assume": assume, "population": population}, ctx)
                rows.append({"reserve_pct": pct, "assume": assume, **strip(result), "nov3_h": overlap(result["union"], [NOV3]) / 3600})
        return rows
    rows_b = table_b_rows("any")  # 説明の (1) の大きさ。判定はこの標本の既定の行で読む
    rows_b_build = table_b_rows("build")  # 追加の比較

    # 感度: 走っている run の数え方と式を変えた行（判定には使わない）
    def variant_rows(population):
        rows = []
        for name, overrides in VARIANTS:
            config = {"reserve_pct": 0.05, "population": population, **overrides}
            result = simulate(config, ctx)
            rows.append({"name": name, "reserve_pct": config["reserve_pct"], **strip(result), "nov3_h": overlap(result["union"], [NOV3]) / 3600})
        return rows
    variants = variant_rows("any")
    variants_build = variant_rows("build")

    # 案の規則の控えと記録の控えの重なり（既定の行）
    default = simulate({"reserve_pct": 0.05, "assume": "zero", "population": "any"}, ctx)
    default_rec = {
        "overlap_h": overlap(default["union"], union_rec) / 3600,
        "only_proposal_h": (total(default["union"]) - overlap(default["union"], union_rec)) / 3600,
    }

    # 窓の組（S3 と S5 の式で。判定には使わない）
    grid = []
    for population in ["any", "build"]:
        for name, overrides in [VARIANTS[2], VARIANTS[4]]:
            for qname, q in [("p90", 0.90), ("p95", 0.95), ("最大", 1.0)]:
                for count in [20, 50]:
                    result = simulate({"reserve_pct": 0.05, "q": q, "count": count, "population": population, **overrides}, ctx)
                    grid.append({"population": population, "name": name[:2], "q": qname, "count": count, **strip(result), "nov3_h": overlap(result["union"], [NOV3]) / 3600})

    # 走っている実際の run の本数の分布
    def counts(at):
        values = [len(v) for v in at]
        return {
            "median": statistics.median(values),
            "max": max(values),
            "zero_share": sum(c == 0 for c in values) / len(values),
            "ge_parallel_share": sum(c >= PARALLEL for c in values) / len(values),
            "nov3_median": statistics.median(c for c, s in zip(values, samples) if NOV3[0] <= s["t"] < NOV3[1]),
            "nov3_zero_share": statistics.mean(c == 0 for c, s in zip(values, samples) if NOV3[0] <= s["t"] < NOV3[1]),
        }
    active_counts = {k: counts(ctx[k]) for k in ["period", "busy", "building"]}

    # 空きが今の閾値のすぐ上に張りつく度合い（空きは今の規則の掃除の結果で、案の規則の下の空きではない）
    ratio_to_need = [s["free"] / (n * CLAIM_FACTOR) for s, n in zip(samples, needs) if s["free"] is not None and n]
    near_threshold = {
        "below": sum(r < 1 for r in ratio_to_need) / len(ratio_to_need),
        "within_25pct_above": sum(1 <= r < 1.25 for r in ratio_to_need) / len(ratio_to_need),
        "median_ratio": statistics.median(ratio_to_need),
    }
    # 11/03 の止まりの想定
    nov3_est = {}
    for population in ["any", "build"]:
        for name, (q, count) in {"p90_20": (0.90, 20), "p95_20": (0.95, 20), "max_20": (1.0, 20), "p90_50": (0.90, 50)}.items():
            sized = Sizes(size_events, population)
            sized.advance(NOV3[0] + 60)
            est = sized.estimate(NOV3[0] + 60, q, count, 14)
            nov3_est[f"{population}_{name}"] = est / GIB

    # 2026-11-03 の止まり
    nov3 = []
    for opened, end, why in claim_spans:
        if opened["payload"].get("reason") == "disk_space" and min(end, PERIOD_END) - opened["t"] > 3600 and in_period(opened["t"]):
            nov3.append(
                {
                    "start": opened["created_at"],
                    "hours": (min(end, PERIOD_END) - opened["t"]) / 3600,
                    "free_gib": opened["payload"]["value"] / GIB,
                    "threshold_gib": opened["payload"]["threshold"] / GIB,
                    "ended_by": why,
                }
            )

    # (6) 同じ型の run
    builds = {}
    for event in size_events:
        if event["kind"] == "build_outputs_removed" and event["run_id"]:
            builds.setdefault(event["run_id"], []).append(event)
    same_type, no_prior = [], []
    for run, events in builds.items():
        for i, event in enumerate(events):
            if not in_period(event["t"]):
                continue
            bytes_ = event["payload"]["bytes"]
            if bytes_ < 20 * GIB:
                continue
            row = {"run": run[:8], "task": event["task_id"], "at": event["created_at"], "gib": bytes_ / GIB, "reason": event["payload"]["reason"]}
            if i == 0:
                no_prior.append(row)
            elif bytes_ >= 2 * events[i - 1]["payload"]["bytes"]:
                row["prev_gib"] = events[i - 1]["payload"]["bytes"] / GIB
                row["prev_at"] = events[i - 1]["created_at"]
                same_type.append(row)
            else:
                row["prev_gib"] = events[i - 1]["payload"]["bytes"] / GIB
                row["note"] = "20 GiB 以上だが前の 2 倍未満"
                no_prior.append(row)

    # 追加の比較: 削除の event の時刻で run を選ぶ分布（期間の前に claim した run を含み、期間の後の記録を含まない）。
    final = {}
    last_build = {}
    for event in size_events:
        run, bytes_ = event["run_id"], event["payload"].get("bytes")
        if run is None or bytes_ is None or not in_period(event["t"]):
            continue
        final.setdefault(run, {})
        final[run][event["kind"]] = max(final[run].get(event["kind"], 0), bytes_)
        if event["kind"] == "build_outputs_removed":
            last_build[run] = event["t"]
    def dist(selected):
        values = sorted(selected)
        if not values:
            return None
        return {
            "runs": len(values),
            "median": statistics.median(values) / GIB,
            "p90": quantile(values, 0.9) / GIB,
            "p95": quantile(values, 0.95) / GIB,
            "p99": quantile(values, 0.99) / GIB,
            "max": values[-1] / GIB,
        }
    sized = {r: sum(k.values()) for r, k in final.items() if "build_outputs_removed" in k}
    dists_event_period = {
        "build": dist(sized.values()),
        "build_before": dist(v for r, v in sized.items() if last_build[r] < SPLIT_224),
        "build_after": dist(v for r, v in sized.items() if last_build[r] >= SPLIT_224),
        "any": dist(sum(k.values()) for k in final.values()),
    }

    # 大きさの分布: 期間内に claim した run の、種類ごとの run の中の最大の和（表 A の実際と同じで、期間の後の記録も含む）。
    # 2026-10-28 の前後は claim の時刻で分ける（表 A と同じ）。
    claimed_at = {r: p[0] for r, p in periods.items() if in_period(p[0])}
    claimed = {}
    for event in size_events:
        run, bytes_ = event["run_id"], event["payload"].get("bytes")
        if run not in claimed_at or bytes_ is None:
            continue
        kinds = claimed.setdefault(run, {})
        kinds[event["kind"]] = max(kinds.get(event["kind"], 0), bytes_)
    def claimed_dist(with_build, before):
        return dist(
            sum(kinds.values())
            for run, kinds in claimed.items()
            if (not with_build or "build_outputs_removed" in kinds)
            and (before is None or (claimed_at[run] < SPLIT_224) == before)
        )
    dists = {
        "any": claimed_dist(False, None),
        "any_before": claimed_dist(False, True),
        "any_after": claimed_dist(False, False),
        "build": claimed_dist(True, None),
        "build_before": claimed_dist(True, True),
        "build_after": claimed_dist(True, False),
    }

    # run_size の差: 種類ごとの最大を run をまたいで足す今の値と、run の中で足す値（期間内に claim した run）
    cross = {k: max((kinds.get(k, 0) for kinds in claimed.values()), default=0) for k in SIZE_KINDS}
    size_diff = {
        "cross_run_sum_gib": sum(cross.values()) / GIB,
        "cross_run_parts_gib": {k: v / GIB for k, v in cross.items()},
        "within_run_max_gib": max(sum(k.values()) for k in claimed.values()) / GIB,
        "cross_vs_within_median_ratio": None,
    }
    # 窓ごと（今の sample_runs の 20 件）の差: claim の時点の今の値 ÷ 同じ 20 件の run の中の和の最大
    ratios = []
    sizes = Sizes(size_events, "any")
    per_kind = {k: [] for k in SIZE_KINDS}
    i = 0
    for t, run in sorted((p[0], r) for r, p in periods.items() if in_period(p[0])):
        while i < len(size_events) and size_events[i]["t"] <= t:
            bytes_ = size_events[i]["payload"].get("bytes")
            if bytes_ is not None:
                per_kind[size_events[i]["kind"]].append(bytes_)
            i += 1
        sizes.advance(t)
        cur = sum(max(v[-SAMPLE_RUNS:]) for v in per_kind.values() if v)
        within = sizes.estimate(t, 1.0, SAMPLE_RUNS * 3, 365)
        if within:
            ratios.append(cur / within)
    size_diff["cross_vs_within_median_ratio"] = statistics.median(ratios)
    size_diff["cross_vs_within_max_ratio"] = max(ratios)

    # 実際の disk_full
    disk_full = [e for e in load(data, "verification_command") if in_period(e["t"]) and isinstance(e["payload"].get("failure"), dict) and e["payload"]["failure"].get("class") == "disk_full"]
    cleanups = [e for e in load(data, "auto_repaired") if in_period(e["t"]) and e["payload"].get("repair") == "disk_cleanup"]
    cleaned = [e["payload"].get("bytes") or 0 for e in cleanups]

    free_values = sorted(s["free"] for s in samples if s["free"] is not None)
    size_all_a = table_a(size_events, periods, "build")
    size_any_a = table_a(size_events, periods, "any")
    summary = {
        "period": {"start": PERIOD_START, "end": PERIOD_END, "hours": period_secs / 3600},
        "samples": len(samples),
        "free_blank": sum(s["free"] is None for s in samples),
        "gap_h": gap_secs / 3600,
        "gap_share": gap_secs / period_secs,
        "gap_count": len(gaps),
        "free_gib": {
            "min": free_values[0] / GIB,
            "median": statistics.median(free_values) / GIB,
            "max": free_values[-1] / GIB,
        },
        "total_gib": samples[0]["total"] / GIB,
        "landing_column_samples": sum(s["has_landing"] for s in samples),
        "landing_value_samples": sum(s["landing"] is not None for s in samples),
        "period_claims": period_claims,
        "median_duration_h": median_duration / 3600,
        "active_counts": active_counts,
        "near_threshold": near_threshold,
        "nov3_est_gib": nov3_est,
        "cleanup_bytes": {"median_gib": statistics.median(cleaned) / GIB, "p90_gib": quantile(cleaned, 0.9) / GIB, "total_gib": sum(cleaned) / GIB},
        "gaps_top": sorted(((b - a) / 3600 for a, b in gaps), reverse=True)[:5],
        "recorded": {
            "claim_h": total(claim_rec) / 3600,
            "landing_h": total(landing_rec) / 3600,
            "union_h": total(union_rec) / 3600,
            "union_in_csv_h": rec_in_csv / 3600,
            "longest_h": max((b - a for a, b in union_rec), default=0) / 3600,
            "claim_spans": len(claim_rec),
            "landing_spans": len(landing_rec),
            "claim_events": sum(1 for s in claim_spans if s[0]["payload"].get("reason") == "disk_space" and in_period(s[0]["t"])),
            "landing_events": sum(1 for s in landing_spans if s[0]["payload"].get("reason") == "disk_space" and in_period(s[0]["t"])),
            "long_holds": nov3,
        },
        "reproduced": {
            "claim_h": total(repro_claim) / 3600,
            "landing_h": total(repro_landing) / 3600,
            "union_h": total(repro_union) / 3600,
            "overlap_h": repro_overlap / 3600,
            "overlap_share_of_recorded": repro_overlap / total(union_rec),
            "total_ratio": total(repro_union) / total(union_rec),
            "agrees": repro_overlap / total(union_rec) >= 0.8 and 0.8 <= total(repro_union) / total(union_rec) <= 1.2,
            "longest_h": max((b - a for a, b in repro_union), default=0) / 3600,
            "threshold_check": threshold_check(claim_spans, size_events),
        },
        "observed_short_samples": len(observed_short),
        "disk_full_events": len(disk_full),
        "disk_cleanups": len(cleanups),
        "table_a_build": size_all_a,
        "table_a_any": size_any_a,
        "table_b": rows_b,
        "table_b_build": rows_b_build,
        "default_vs_recorded": default_rec,
        "variants": variants,
        "variants_build": variants_build,
        "grid": grid,
        "same_type": same_type,
        "no_prior": no_prior,
        "dists": dists,
        "dists_event_period": dists_event_period,
        "size_diff": size_diff,
    }
    with open(out_path, "w") as f:
        json.dump(summary, f, ensure_ascii=False, indent=1)


if __name__ == "__main__":
    main()
