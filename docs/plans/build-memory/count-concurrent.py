"""cgu-runs.csv の各 run と同時に動いていた本番の worker の turn と verification を数える
（標準ライブラリだけ）。数え方は docs/plans/build-memory.md の「条件」と同じで、測定の後に
本番 queue の各 run の dir のファイルの時刻から区間を作り直す。

Usage: python3 -I docs/plans/build-memory/count-concurrent.py <runs の dir> <自分の run id> \
           [cgu-runs.csv] > docs/plans/build-memory/cgu-concurrent.csv

区間:
  worker の turn: turns/turn-N.err の作成から turns/turn-N.jsonl の最後の書き込みまで。
  verification: integrate-N-verify-M.log の最後の書き込みを終わりとし、同じ integrate の
    1 つ前のコマンドのログ（M-1）の最後の書き込みを始まりにする（M=1 は自分の作成）。
    同じ integrate のコマンドは重ならないので、数は同時に動いていた integrate の数になる。
出す列: cgu-runs.csv の行の順（流した順）に、開始と終了の時点の数と、run の間を 60 秒
  ごとに数えた最大と平均。自分の run の dir は除く。
"""

import csv
import datetime
import glob
import os
import re
import sys


def ts(s):
    return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(
        tzinfo=datetime.timezone.utc).timestamp()


def intervals(runs_dir, own):
    turns, verifies = [], []
    for d in glob.glob(os.path.join(runs_dir, "*")):
        if os.path.basename(d) == own or not os.path.isdir(d):
            continue
        for err in glob.glob(os.path.join(d, "turns", "turn-*.err")):
            jsonl = err[:-4] + ".jsonl"
            if os.path.exists(jsonl):
                turns.append((os.stat(err).st_birthtime, os.stat(jsonl).st_mtime))
        logs = {}
        for p in glob.glob(os.path.join(d, "integrate-*-verify-*.log")):
            m = re.search(r"integrate-(\d+)-verify-(\d+)\.log$", p)
            if m:
                logs[(int(m.group(1)), int(m.group(2)))] = p
        for (n, k), p in logs.items():
            prev = logs.get((n, k - 1))
            start = os.stat(prev).st_mtime if prev else os.stat(p).st_birthtime
            verifies.append((start, os.stat(p).st_mtime))
    return turns, verifies


def count(ivs, t):
    return sum(1 for a, b in ivs if a <= t <= b)


def main():
    runs_dir, own = sys.argv[1], sys.argv[2]
    path = sys.argv[3] if len(sys.argv) > 3 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "cgu-runs.csv")
    with open(path, newline="") as f:
        rows = list(csv.DictReader(f))
    turns, verifies = intervals(runs_dir, own)
    w = csv.writer(sys.stdout, lineterminator="\n")
    w.writerow(["round", "variant", "target", "turns_start", "verify_start", "turns_end",
                "verify_end", "turns_max", "verify_max", "turns_mean", "verify_mean"])
    for r in rows:
        a, b = ts(r["start_utc"]), ts(r["end_utc"])
        points = [a + 60 * i for i in range(int((b - a) // 60) + 1)] + [b]
        tc = [count(turns, t) for t in points]
        vc = [count(verifies, t) for t in points]
        w.writerow([r["round"], r["variant"], r["target"], tc[0], vc[0], tc[-1], vc[-1],
                    max(tc), max(vc), f"{sum(tc) / len(tc):.1f}", f"{sum(vc) / len(vc):.1f}"])


if __name__ == "__main__":
    main()
