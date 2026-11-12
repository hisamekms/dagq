#!/usr/bin/env python3
"""summary.json から文書の表（Q1 の内訳・Q4 の足し方ごと）を Markdown で出す。使い方: render.py SUMMARY_JSON"""
import json
import sys

s = json.load(open(sys.argv[1]))


def g(x):
    return f"{x:.1f}"


print("| claim_held の時刻（UTC） | 期間 | 空き | 必要量 | 最低ライン | 想定（標本） | 動いている run | 着地の検証中 | 着地待ちの run | 着地待ちの想定 | 残り（差） |")
print("|---|---|---|---|---|---|---|---|---|---|---|")
for r in s["q1"]:
    print(
        f"| {r['at'][:19]} | {'T2 前' if r['before_t2'] else 'T2 後'} | {g(r['free_gib'])} | {g(r['threshold_gib'])} | {g(r['reserve_gib'])} "
        f"| {g(r['expected_gib'])}（{r['samples']}） | {r['moving']} | {r['landing_in_progress']} | {r['landing_waiting_runs']} "
        f"| {g(r['landing_added_gib'])} | {g(r['remaining_gib'])} |"
    )
print()
fails = s["observed_failures"]
observed = fails["disk_full_verification"] + fails["full_disk_resume"]
missing = s["period_missing_h"]  # [T, E) の標本期間全体の欠測（控えの区間の中の欠測は判定の節が別に持つ）


def table(rows):
    print("| 足し方 | 控えの時間（h） | 欠測の時間（h） | 容量不足の区間の数 | 仮想の run の数 | claim の数（run_claimed + 仮想） | 比 | 観測した容量不足の失敗（a〜d 共通） |")
    print("|---|---|---|---|---|---|---|---|")
    for w in rows:
        print(
            f"| {w['way']} | {w['hold_h']:.2f} | {missing:.2f} | {w['short_regions']} | {w['virtual_runs']} | {w['claims']} "
            f"| {w['ratio'] * 100:.1f}% | {observed}（disk_full {fails['disk_full_verification']}・やり直し {fails['full_disk_resume']}） |"
        )


table(s["q4"])
print()
table(s["q4_sensitivity_pre_t2_remaining_zero"])
