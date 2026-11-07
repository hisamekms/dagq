#!/usr/bin/env python3
"""summary.json から文書の表を Markdown で出す。使い方: render.py SUMMARY_JSON"""
import json
import sys

s = json.load(open(sys.argv[1]))


def n(x, d=1):
    return "—" if x is None else f"{x:.{d}f}"


def pct(x):
    return "—" if x is None else f"{x * 100:.1f}%"


def table_a(rows, part):
    print("| 分位点 | 件数 | 期間（日） | 想定のあった run 数 | 超えた率 | 超えた量の中央値（GiB） | 超えた量の最大（GiB） | 想定の中央値（GiB） |")
    print("|---|---|---|---|---|---|---|---|")
    for r in rows:
        v = r[part]
        print(f"| {r['q']} | {r['count']} | {r['days']} | {v['runs']} | {pct(v['over_rate'])} | {n(v['over_median_gib'])} | {n(v['over_max_gib'])} | {n(v['est_median_gib'])} |")


def rule_row(label, r):
    return f"{label} | {n(r['claim_h'])} | {n(r['landing_h'])} | {n(r['union_h'])} | {n(r['longest_h'])} | {r['short_regions']} | {r['short_samples']} | {r['claims']} | {pct(r['ratio'])} |"


def table_b(rows):
    print("| 規則 | reserve | 今の大きさの仮定 | claim の控え（h） | 着地の控え（h） | 和集合の合計（h） | 最長の控え（h） | 容量不足の区間数 | 容量不足の sample 数 | claim の数 | 比 |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    rec = s["recorded"]
    print(f"| 今の規則（観測の空きのまま・記録の控え） | — | — | {n(rec['claim_h'])} | {n(rec['landing_h'])} | {n(rec['union_h'])} | {n(rec['longest_h'])} | 0 | {s['observed_short_samples']} | {s['period_claims']} | 0.0% |")
    names = {"zero": "想定そのまま（今の大きさ 0）", "linear": "claim から想定へ線形"}
    for r in rows:
        label = f"| 案の規則 | max(5 GiB, {r['reserve_pct'] * 100:.0f}%) | {names[r['assume']]}"
        print(rule_row(label, r))


def variants(rows):
    print("| 式 | claim の控え（h） | 着地の控え（h） | 和集合の合計（h） | 最長の控え（h） | 容量不足の区間数 | 容量不足の sample 数 | claim の数 | 比 | 仮想の run | 判断の必要量の中央値（GiB） | 11/03 の止まりの中の控え（h） |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for r in rows:
        print(rule_row(f"| {r['name']}", r) + f" {r['virtual_claims']} | {n(r['need_median_gib'])} | {n(r['nov3_h'])} |")


def grid(population):
    print("| 式 | 分位点 | 件数 | 和集合の合計（h） | 容量不足の区間数 | 比 | 11/03 の止まりの中の控え（h） |")
    print("|---|---|---|---|---|---|---|")
    for r in s["grid"]:
        if r["population"] == population:
            print(f"| {r['name']} | {r['q']} | {r['count']} | {n(r['union_h'])} | {r['short_regions']} | {pct(r['ratio'])} | {n(r['nov3_h'])} |")


for title, rows, part in [
    ("A_ANY", "table_a_any", "all"),
    ("A_BUILD", "table_a_build", "all"),
    ("A_ANY_BEFORE", "table_a_any", "before"),
    ("A_ANY_AFTER", "table_a_any", "after"),
    ("A_BUILD_BEFORE", "table_a_build", "before"),
    ("A_BUILD_AFTER", "table_a_build", "after"),
]:
    print(f"## {title}\n")
    table_a(s[rows], part)
    print()
def dist_row(label, v):
    print(f"| {label} | {v['runs']} | {n(v['median'])} | {n(v['p90'])} | {n(v['p95'])} | {n(v['p99'])} | {n(v['max'])} |")


DIST_HEAD = "| 範囲 | run 数 | 中央値 | p90 | p95 | p99 | 最大 |\n|---|---|---|---|---|---|---|"
d = s["dists"]
print("## D_CLAIMED\n")
print(DIST_HEAD)
dist_row("全 run", d["any"])
dist_row("全 run・2026-10-28 より前に claim", d["any_before"])
dist_row("全 run・2026-10-28 以後に claim", d["any_after"])
dist_row("build の記録のある run", d["build"])
dist_row("build の記録のある run・2026-10-28 より前に claim", d["build_before"])
dist_row("build の記録のある run・2026-10-28 以後に claim", d["build_after"])
e = s["dists_event_period"]
print("\n## D_EVENT\n")
print(DIST_HEAD)
dist_row("全 run", e["any"])
dist_row("build の記録のある run", e["build"])
dist_row("build の記録のある run・最後の記録が 2026-10-28 より前", e["build_before"])
dist_row("build の記録のある run・最後の記録が 2026-10-28 以後", e["build_after"])
print()
print("## B_ANY\n")
table_b(s["table_b"])
print("\n## B_BUILD\n")
table_b(s["table_b_build"])
print("\n## S_ANY\n")
variants(s["variants"])
print("\n## S_BUILD\n")
variants(s["variants_build"])
print("\n## G_ANY\n")
grid("any")
print("\n## G_BUILD\n")
grid("build")
