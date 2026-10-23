"""docs/plans/build-memory.md の表を runs.csv から作る（標準ライブラリだけ）。

Usage: python3 -I docs/plans/build-memory/summarize.py [runs.csv]

出すもの（markdown）:
  - 周ごとの表: runs.csv の 1 行ずつ（流した順）。
  - 集計の表: 対象・variant ごとに、1 process の peak と合計の peak は 2 周の大きい方、
    時間は 2 周の平均。下げる手の効きは、同じ対象の V0（(iii) は T6）に対する
    peak の比と時間の比（どちらも単位なし）。
  - container との比: note 297078 の container の rustc の anon-rss 3856512 KiB を、
    host の V0 の (i) の 1 process の peak（KiB）で割った値。
"""

import csv
import os
import sys

CONTAINER_ANON_RSS_KIB = 3856512
TARGETS = [("build", "(i) cargo build --locked --lib", "V0"),
           ("test-build", "(ii) cargo test --locked --no-run", "V0"),
           ("unit", "(iii) cargo nextest run --locked --lib --bins", "T6")]
ORDER = ["V0", "V1", "V2", "V3", "V4", "V5", "V6", "T6", "T2"]


def mib(kib):
    return f"{int(kib) / 1024:,.0f}"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "runs.csv")
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))

    print("### 周ごと（流した順）\n")
    print("| # | 周 | variant | 対象 | 開始（UTC） | 終了（UTC） | 秒 | exit "
          "| 1 process の peak（KiB） | その process | 合計の peak（KiB） "
          "| time -l の max RSS（byte） | load 1 分（前→後） |")
    print("| ---: | ---: | --- | --- | --- | --- | ---: | ---: | ---: | --- | ---: | ---: | --- |")
    for i, r in enumerate(rows, 1):
        print(f"| {i} | {r['round']} | {r['variant']} | {r['target']} | {r['start_utc']} "
              f"| {r['end_utc']} | {r['seconds']} | {r['exit']} | {r['peak_proc_kib']} "
              f"| {r['peak_proc_name']} | {r['peak_total_kib']} | {r['time_maxrss_bytes']} "
              f"| {r['load1_start']}→{r['load1_end']} |")

    agg = {}
    for r in rows:
        a = agg.setdefault((r["target"], r["variant"]), [])
        a.append(r)

    print("\n### 集計\n")
    print("peak は 2 周の大きい方、時間は 2 周の平均。\n"
          "比は同じ対象の基準（(i)・(ii) は V0、(iii) は T6）に対する値で単位なし。\n"
          "MiB は KiB ÷ 1024 を丸めた読みやすさのための値。\n")
    print("| 対象 | variant | 1 process の peak（KiB） | MiB | 合計の peak（KiB） | MiB "
          "| time -l の max RSS の大きい方（byte） | 時間の平均（秒） "
          "| 1 process の peak ÷ 基準 | 合計の peak ÷ 基準 | 時間 ÷ 基準 |")
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for target, label, base in TARGETS:
        def stat(variant):
            rs = agg.get((target, variant))
            if not rs:
                return None
            return (max(int(r["peak_proc_kib"]) for r in rs),
                    max(int(r["peak_total_kib"]) for r in rs),
                    max(int(r["time_maxrss_bytes"] or 0) for r in rs),
                    sum(int(r["seconds"]) for r in rs) / len(rs), len(rs))
        b = stat(base)
        for v in ORDER:
            s = stat(v)
            if s is None:
                continue
            proc, total, maxrss, secs, n = s
            note = "" if n == 2 else f"（{n} 周）"
            print(f"| {label} | {v}{note} | {proc} | {mib(proc)} | {total} | {mib(total)} "
                  f"| {maxrss} | {secs:.1f} | {proc / b[0]:.2f} | {total / b[1]:.2f} "
                  f"| {secs / b[3]:.2f} |")

    b = agg.get(("build", "V0"))
    if b:
        proc = max(int(r["peak_proc_kib"]) for r in b)
        print(f"\n### container との比\n\n{CONTAINER_ANON_RSS_KIB} KiB ÷ {proc} KiB "
              f"= {CONTAINER_ANON_RSS_KIB / proc:.2f}")


if __name__ == "__main__":
    main()
