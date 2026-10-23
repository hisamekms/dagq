"""docs/plans/codegen-units-spike.md の表と判定を cgu-runs.csv と cgu-concurrent.csv から
作る（標準ライブラリだけ）。

Usage: python3 -I docs/plans/build-memory/summarize-cgu.py [cgu-runs.csv [cgu-concurrent.csv]]

出すもの（markdown）:
  - 周ごとの表: cgu-runs.csv の 1 行ずつ（流した順）。cgu-concurrent.csv があれば同じ行の
    本番の worker の turn と verification の数を添える。
  - 比の表: 周ごとに同じ周の V0 と V5 を組にした比 V5/V0（(i)・(ii) の 1 process の peak・
    合計の peak・時間）と、4 周の比の中央値（2 番目と 3 番目の平均）。
  - 判定: codegen-units-spike.md の「判定」の規則（測る前に決めたもの）を当てはめた結果。
    exit が 0 でない run のある組は、その周を欠けたものとして扱う。
"""

import csv
import os
import sys

TARGETS = [("build", "(i)"), ("test-build", "(ii)")]
METRICS = [("peak_proc_kib", "1 process の peak"), ("peak_total_kib", "合計の peak"),
           ("seconds", "時間")]


def median4(xs):
    s = sorted(xs)
    return (s[1] + s[2]) / 2 if len(s) == 4 else None


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "cgu-runs.csv")
    cpath = sys.argv[2] if len(sys.argv) > 2 else os.path.join(here, "cgu-concurrent.csv")
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))
    conc = []
    if os.path.exists(cpath):
        with open(cpath, newline="") as f:
            conc = list(csv.DictReader(f))

    print("### 周ごと（流した順）\n")
    print("| # | 周 | variant | 対象 | 開始（UTC） | 終了（UTC） | 秒 | exit "
          "| 1 process の peak（KiB） | その process | 合計の peak（KiB） "
          "| time -l の max RSS（byte） | load 1 分（前→後） "
          "| worker の turn（前→後、最大） | verification（前→後、最大） |")
    print("| ---: | ---: | --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: "
          "| --- | --- | --- |")
    for i, r in enumerate(rows, 1):
        c = conc[i - 1] if i <= len(conc) else None
        tw = f"{c['turns_start']}→{c['turns_end']}、{c['turns_max']}" if c else ""
        vw = f"{c['verify_start']}→{c['verify_end']}、{c['verify_max']}" if c else ""
        print(f"| {i} | {r['round']} | {r['variant']} | {r['target']} | {r['start_utc']} "
              f"| {r['end_utc']} | {r['seconds']} | {r['exit']} | {r['peak_proc_kib']} "
              f"| {r['peak_proc_name']} | {r['peak_total_kib']} | {r['time_maxrss_bytes']} "
              f"| {r['load1_start']}→{r['load1_end']} | {tw} | {vw} |")

    # 流し直した組は後の行が勝つ
    by = {}
    for r in rows:
        by[(r["round"], r["variant"], r["target"])] = r
    rounds = sorted({r["round"] for r in rows}, key=int)
    ratios = {}  # (target, metric) -> [ratio per complete round]
    complete = []
    for rd in rounds:
        pair = {(v, t): by.get((rd, v, t)) for v in ("V0", "V5") for t, _ in TARGETS}
        if any(x is None or x["exit"] != "0" for x in pair.values()):
            continue
        complete.append(rd)
        for t, _ in TARGETS:
            for m, _ in METRICS:
                ratios.setdefault((t, m), []).append(
                    int(pair[("V5", t)][m]) / int(pair[("V0", t)][m]))

    print("\n### 周ごとの比 V5/V0 と 4 周の中央値\n")
    print("比は同じ周の V5 の値 ÷ V0 の値（単位なし）。中央値は 4 周の比の 2 番目と 3 番目の平均。\n")
    print("| 対象 | 値 | " + " | ".join(f"{rd} 周目" for rd in complete)
          + " | 1 未満の周 | 中央値 |")
    print("| --- | --- | " + " | ".join("---:" for _ in complete) + " | ---: | ---: |")
    for t, tl in TARGETS:
        for m, ml in METRICS:
            xs = ratios.get((t, m), [])
            med = median4(xs)
            print(f"| {tl} | {ml} | " + " | ".join(f"{x:.2f}" for x in xs)
                  + f" | {sum(1 for x in xs if x < 1)} | "
                  + (f"{med:.2f}" if med is not None else "（4 周がそろわない）") + " |")

    print("\n### 判定に当てはめた値\n")
    tot = ratios.get(("test-build", "peak_total_kib"), [])
    if len(complete) != 4:
        print(f"unresolved: そろった周は {len(complete)} 周で、4 周がそろわない。")
        return
    below = sum(1 for x in tot if x < 1)
    mt = median4(tot)
    m1 = median4(ratios[("build", "seconds")])
    m2 = median4(ratios[("test-build", "seconds")])
    holds = below >= 3 and mt <= 0.95 and m1 <= 1.0 and m2 <= 1.0
    dnh = mt >= 1.0 or below <= 1 or m1 > 1.05 or m2 > 1.05
    verdict = "holds" if holds else "does_not_hold" if dnh else "unresolved"
    print(f"- (ii) の合計の peak の比: 1 未満の周 {below} / 4、中央値 {mt:.3f}")
    print(f"- 時間の比の中央値: (i) {m1:.3f}、(ii) {m2:.3f}")
    print(f"- 判定: {verdict}")


if __name__ == "__main__":
    main()
