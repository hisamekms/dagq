---
id: plan-claim-spacing
type: plan
title: claim の間隔の導入前後の load と保留の比較
status: completed
created: 2026-10-05
related:
  - adr-t1479-1
  - adr-0049
---

# claim の間隔の導入前後の測定

新しい claim の間を既定 180 秒空ける変更の後、短時間の連続 claim は無くなり、1 本の組の claim 後 6 分の load1 の最大の中央値は 15.68 から 10.53 に下がった。ただし後の期間にも load_average の保留が残り、finding 36 の resolve はこの測定では勧めない。直近 1 時間の観測も欠測があり、完全な 1 時間の条件達成は確認できない。

## 境目と期間

読み取り専用の固定バイナリ `~/.local/bin/dagq` を worktree 内で使用。対象は [ADR-t1479-1](../adr/2026-10-04-t1479-1-space-new-claims-while-the-load-hold-is-on.md) の commit `eeb575c60513910128a1afd868548def0ad4253c`。build の照合は 2026-10-05T08:32:48Z を打ち切りとして、次のコマンドを `--after` の cursor で空のページまで繰り返した（9 ページ、727 event、376 build）。version の `+` の後ろを commit として、全 build に `git merge-base --is-ancestor eeb575c6 <commit>` を実行。exit 0 は 64 build、exit 1 は 312 build、その他は無い。

```sh
dagq events --full --kind update_installed --kind supervisor_started --until 2026-10-05T08:32:48Z --after 0 --limit 100
# 次のページは返った cursor を --after に渡す。
git merge-base --is-ancestor eeb575c6 eeb575c60513910128a1afd868548def0ad4253c
```

| event id | kind | created_at (UTC) | commit | merge-base exit |
| --- | --- | --- | --- | --- |
| 78813 | supervisor_started | 2026-10-03T20:35:58.101Z | f549a9580b80ea33ba5ffe0fb4c9476f4db3da25 | 1 |
| 78816 | update_installed | 2026-10-03T20:36:00.301Z | f549a9580b80ea33ba5ffe0fb4c9476f4db3da25 | 1 |
| **78919** | **supervisor_started** | **2026-10-03T20:40:51.889Z** | eeb575c60513910128a1afd868548def0ad4253c | **0** |
| 78932 | update_installed | 2026-10-03T20:40:53.910Z | eeb575c60513910128a1afd868548def0ad4253c | 0 |

exit 0 の最初の event 78919 を境目とした。KPI の `compare.split.marks` にも同じ build の印 **78919** がある。

測定開始は **2026-10-05T08:33:50Z**。その時点の host CSV の最後の行は `unix=1791189213`、**2026-10-05T08:33:33Z**（load1=1.62）。早い方を観測の終わりに固定した。CSV は queue dir `/Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/host/metrics-YYYYMMDD.csv` の 20260930〜20261005 を読む（ファイル名の日付は host の local 日付、集計は unix による UTC）。

| 期間 | 左端（含む、UTC） | 右端（含まない、UTC） | 長さ | run_claimed |
| --- | --- | --- | --- | --- |
| 前 | 2026-09-30T20:40:51.889Z | 2026-10-03T20:40:51.889Z | 72 時間 | 187 件 |
| 後 | 2026-10-03T20:40:51.889Z | 2026-10-05T08:33:33Z | 35 時間 52 分 41.111 秒 | 131 件 |

後は 30 claim・24 時間の両方を超え、標本数・期間不足による仮の結論には当たらない。ただし欠測と他の変更の影響は残る。

## claim の組と load1

各期間を個別に created_at 昇順で並べ、直前との間隔が 10 秒以下なら同じ組へ鎖でつなぐ。組の先頭の claim で期間を決める。前は 168 組（187 claim）、後は 131 組（131 claim）。

claim 時の値は `unix <= t_first` の最新の行（60 秒を越えて古ければ欠測）。ピークは `t_first < unix <= t_last + 360` の load1 最大（10 行未満なら欠測）。`t_last + 360 > 観測の終わり` は先に不完全な窓として除く。前の期間の末尾の窓も、境目を越えて観測の終わりまで読める。欠測の組は両方の値から除く。中央値は昇順の中央、偶数なら中央 2 値の平均、小数 2 桁。人の答え待ちは引かない。

| 期間・組の大きさ | 全組数（n=分類した組） | 採用組数（n=全組） | claim 時 load1 中央値 | 6 分ピーク中央値 | ピーク >16 の組 |
| --- | --- | --- | --- | --- | --- |
| 前・1 本 | 151 (n=168) | 112 (n=151) | 10.14 (n=112) | 15.68 (n=112) | 53 (n=112) |
| 前・2 本以上 | 17 (n=168) | 14 (n=17) | 7.46 (n=14) | 20.01 (n=14) | 9 (n=14) |
| 後・1 本 | 131 (n=131) | 105 (n=131) | 7.49 (n=105) | 10.53 (n=105) | 24 (n=105) |
| 後・2 本以上 | — (n=0) | — (n=0) | — (n=0) | — (n=0) | — (n=0) |

| 期間・組の大きさ | claim 時の欠測 | 窓の欠測 | 不完全な窓 | 除外した異なる組 |
| --- | --- | --- | --- | --- |
| 前・1 本 | 3 (n=151) | 38 (n=151) | 0 (n=151) | 39 (n=151) |
| 前・2 本以上 | 0 (n=17) | 3 (n=17) | 0 (n=17) | 3 (n=17) |
| 後・1 本 | 7 (n=131) | 20 (n=131) | 2 (n=131) | 26 (n=131) |
| 後・2 本以上 | — (n=0) | — (n=0) | — (n=0) | — (n=0) |

claim 時と窓の欠測は両方該当すれば両方に数える（前の 1 本で 2 組、後の 1 本で 3 組重複）。不完全な窓は欠測検査の前に除くため重複しない。欠測を load1=0 として補わない。後に複数 claim の組が無いことは観測された挙動であり、その群の load の改善量は推定できない。

これらの表は「再集計」の script で出した。各期間の run_claimed は下記の引数を使用し、`--after 0 --limit 100` から返された cursor を渡し、空のページまで読む（各期間 3 ページ）。CSV は script 内の bisect で左開き・右閉じを実装している。

```sh
dagq events --kind run_claimed --full --since 2026-09-30T20:40:51.889Z --until 2026-10-03T20:40:51.889Z --after 0 --limit 100
dagq events --kind run_claimed --full --since 2026-10-03T20:40:51.889Z --until 2026-10-05T08:33:33Z --after 0 --limit 100
```

## load_average の保留と間隔の待ち

```sh
dagq stats --full --since 2026-09-30T20:40:51.889Z --until 2026-10-03T20:40:51.889Z
dagq stats --full --since 2026-10-03T20:40:51.889Z --until 2026-10-05T08:33:33Z
```

| 期間 | claim_holds.by_reason.load_average.count | secs |
| --- | --- | --- |
| 前 | 430 (n=430 保留) | 90035 (n=430 保留) |
| 後 | 207 (n=207 保留) | 27880 (n=207 保留) |

`stats` の定義どおり、期間内に始まった保留の数と、右端までに切った秒を読む。長さが異なるので合計だけで効果を判断しない。秒 / 期間長は前 34.74%、後 21.59%。これは窓をまたいで始まった保留を含む host の全保留率ではない。

後の run_claimed の `payload.claim_spacing_wait_secs` は、欄あり **131 件**、うち実測 0 が **106 件**、欄なし **0 件**（各 count の n=131 claim）。欄ありだけの合計 **3051 秒 (n=131)**、中央値 **0.00 秒 (n=131)**。欄なしを 0 として数えていない。前の欄は集計しない。コマンドは上の後の run_claimed の取得、計算は再集計 script の `waits`。

## KPI の補助比較

```sh
dagq kpi --compare 78919 --area runtime
```

既定の 7 日ずつの比較（前 2026-09-26T20:40:51.889Z〜境目、後 境目〜2026-10-10T20:40:51.889Z）なので、上の主測定と混ぜない。後は取得時点で partial、判定は `judged=false, reason=partial`。runtime の verify 中央値は 496 秒 (n=336) → 643 秒 (n=70)、wait_to_land は 915 秒 (n=336) → 1635 秒 (n=70)。ピーク改善が着地時間改善まで示したとは言えない。supervisor/build の更新、並列数の変更（前の一部は 1、境目は 3）、provider/version の変更などが同時にあり、claim の間隔だけの因果効果を切り出した実験ではない。

## finding 36 の判定と次の打ち手

resolve の条件は「1 時間 load_average の保留が無く、1 分の load が --max-load 未満」。閾値は 16。後の期間全体には 207 回の保留と、採用した 1 本の組の 24 / 105 件のピーク >16 があり、持続して条件を満たしたとは言えない。

直近 1 時間も切り出して確認した。

```sh
dagq stats --full --since 2026-10-05T07:33:33Z --until 2026-10-05T08:33:33Z
dagq events --full --kind claim_held --kind claim_resumed --since 2026-10-05T06:33:33Z --until 2026-10-05T08:33:33Z --after 0 --limit 100
```

直近 1 時間の load_average は count=0 (n=0 保留)、secs=0 (n=0 保留)。直前の load_average の再開は event 102718、2026-10-05T07:16:12.656Z で、開始時点へ持ち越した load 保留も無い。CSV の観測標本の最大は **14.42 (n=86)**、16 以上は **0 (n=86)**、最後は **1.62 (n=1)**。ただし最初の行は **2026-10-05T07:49:55Z** で、1 時間の冒頭 **982 秒**の標本が無い。標本の間は最長 36 秒で、その範囲では 16 未満だが、完全な 1 時間の load1 条件を確認できない。したがって finding 36 は **resolve を保留**する。worker は finding を操作しない。

次は、完全な 1 時間の CSV が揃う負荷のある時間帯に、load 保留ゼロと全 load1 標本 <16 を再確認する。引き続き越えるなら、claim 以外の負荷（resume・着地の検証・headless job）と重なった時刻を分けて測り、並列数や test の並列度を下げるかを判断する材料にする。この観測と判断を receipt の improvement follow-up に渡す。claim の間隔を伸ばすことは、この比較だけでは決めない。

## 再集計

以下の本文を `$TMPDIR/claim-spacing.py` に保存し、repository 内で実行する。材料の event と CSV が保持されている間は同じ期間を読み直せる（host CSV の既定保持は 30 日）。途中の header は飛ばし、同じ unix 秒は最後の行を採用する。採用した列は unix と load1 だけ。既存の CSV を入力として使い、新しい store は作らない。

```sh
python3 "$TMPDIR/claim-spacing.py" /Users/shinnosukeooyama/.local/share/dagq/77067154921b9014/host 2026-09-30T20:40:51.889Z 2026-10-03T20:40:51.889Z 2026-10-05T08:33:33Z
```

```python
import bisect
import csv
import glob
import json
import subprocess
import sys
from datetime import datetime, timedelta
from decimal import Decimal
from statistics import median

host, start, boundary, end = sys.argv[1:]

def ts(value):
    return datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()

def cli(*args):
    return json.loads(subprocess.check_output(['dagq', *args]))

def claims(a, b):
    result, cursor = [], 0
    while True:
        page = cli('events', '--kind', 'run_claimed', '--full',
                   '--since', a, '--until', b, '--after', str(cursor),
                   '--limit', '100')
        if not page['events']:
            break
        result.extend(page['events'])
        cursor = page['cursor']
    return sorted(result, key=lambda e: (e['created_at'], e['id']))

def fmt(values):
    return {'median': f'{median(values):.2f}' if values else '—', 'n': len(values)}

rows = {}
for path in sorted(glob.glob(host + '/metrics-*.csv')):
    with open(path) as f:
        for row in csv.DictReader(f):
            if row['unix'] == 'unix':  # schema changes or concurrent writers repeat the header
                continue
            unix = int(row['unix'])
            if ts(start) - 60 <= unix <= ts(end):
                rows[unix] = Decimal(row['load1'])
times = sorted(rows)
loads = [rows[t] for t in times]
output = {}
for name, a, b in [('before', start, boundary), ('after', boundary, end)]:
    events = claims(a, b)
    groups = []
    for event in events:
        t = ts(event['created_at'])
        if groups and t - groups[-1][-1] <= 10:
            groups[-1].append(t)
        else:
            groups.append([t])
    period = {'claims': len(events), 'groups': {},
              'load_average': cli('stats', '--full', '--since', a, '--until', b)
                  ['claim_holds']['by_reason'].get('load_average', {'count': 0, 'secs': 0})}
    for label, multi in [('single', False), ('multiple', True)]:
        selected = [g for g in groups if (len(g) >= 2) == multi]
        at_claim, peaks = [], []
        excluded = {'claim_missing': 0, 'window_missing': 0, 'incomplete': 0}
        for group in selected:
            first, last = group[0], group[-1]
            if last + 360 > ts(end):
                excluded['incomplete'] += 1
                continue
            i = bisect.bisect_right(times, first) - 1
            missing_claim = i < 0 or first - times[i] > 60
            window = loads[bisect.bisect_right(times, first):
                           bisect.bisect_right(times, last + 360)]
            missing_window = len(window) < 10
            excluded['claim_missing'] += int(missing_claim)
            excluded['window_missing'] += int(missing_window)
            if missing_claim or missing_window:
                continue
            at_claim.append(loads[i])
            peaks.append(max(window))
        period['groups'][label] = {
            'total': len(selected), 'included': len(peaks), 'excluded': excluded,
            'at_claim': fmt(at_claim), 'peak': fmt(peaks),
            'over16': {'count': sum(p > 16 for p in peaks), 'n': len(peaks)}}
    if name == 'after':
        waits = [Decimal(str(e['payload']['claim_spacing_wait_secs']))
                 for e in events if 'claim_spacing_wait_secs' in e['payload']]
        period['waits'] = {'present': len(waits), 'zero': waits.count(0),
                           'absent': len(events) - len(waits),
                           'sum': str(sum(waits)), **fmt(waits)}
    output[name] = period
hour_start = (datetime.fromisoformat(end.replace('Z', '+00:00'))
              - timedelta(hours=1)).isoformat().replace('+00:00', 'Z')
hour_times = [t for t in times if ts(hour_start) <= t <= ts(end)]
hour_loads = [rows[t] for t in hour_times]
output['last_hour'] = {
    'since': hour_start, 'until': end,
    'load_average': cli('stats', '--full', '--since', hour_start, '--until', end)
        ['claim_holds']['by_reason'].get('load_average', {'count': 0, 'secs': 0}),
    'load1_max': str(max(hour_loads)), 'n': len(hour_loads),
    'load1_ge16': sum(v >= 16 for v in hour_loads),
    'first_unix': hour_times[0], 'missing_start_secs': hour_times[0] - ts(hour_start),
    'max_gap_secs': max(b - a for a, b in zip(hour_times, hour_times[1:])),
    'last_load1': str(hour_loads[-1])}
print(json.dumps(output, ensure_ascii=False, indent=2))
```
