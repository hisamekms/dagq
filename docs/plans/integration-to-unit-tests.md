---
id: plan-integration-to-unit-tests
type: plan
title: 判断を unit test に移した着地（goal 68、task 1412〜1416）の前後の本番の coverage の関門の test の時間と Summary
status: active
created: 2026-10-03
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-slow-test-waits
  - design-slow-tests
  - plan-nextest-test-threads
---

# 判断を unit test に移した着地（goal 68、task 1412〜1416）の前後の本番の coverage の関門の test の時間と Summary

goal 68 の 2026-10-02 の追加（副作用のない判断を src の関数に切り出して unit test で確かめ、integration test を境界に絞る）の効果を、依頼のときと同じ方法で本番の coverage の関門の log から測った（task 1417）。前後とも 20 本そろった比較で、暫定ではない。src/ と tests/ は変えていない。

## 要点

- **前 20 本・後 20 本（どちらも 6 並列）で、log ごとの test の時間の合計の中央値は 2,205.3→2,533.4 秒（+328.1 秒、+14.9%）、Summary の中央値は 376.8→440.3 秒（+63.5 秒、+16.9%）と、全体では縮まずに伸びた。** 同じ期間に load1 の test 段の平均の中央値は 14.26→17.96、最大の中央値は 18.66→21.20 と上がり、区間の間に dagq::it などで新しく出た test が 64 本・約 250 秒ある。大半は別の着地が足した integration test（review_subagents・runtime_background*・runtime_recheck・runtime_cleanup など）で、1412〜1415 が名前を変えた・足した約 4 本・約 12 秒を含む。
- **変えた 6 module（runtime_resume・runtime_integrate・runtime_review・runtime_review_concern・runtime_job_verdicts・runtime_triage）の合計は縮んだ。** log ごとの中央値で 430.3→333.6 秒（−96.7 秒、−22.5%）、test ごとの中央値の合計で 432.3→333.5 秒（127→100 本、−98.8 秒）。dagq::it の中の割合は 20.7%→13.6%。
- **load の近い log どうし（load1 の平均 12〜17、前 8 本・後 7 本、中央値 15.66 と 15.09）では、変えた 6 module は 445.3→309.3 秒（test ごと中央値の合計、−136.0 秒、−30.5%）、log ごとの中央値で 450.6→313.7 秒（−136.9 秒）。** ただし全体の合計は 2,205.3→2,311.7 秒、Summary は 376.8→393.8 秒で、ここでも縮んでいない。増えた test の分が上回った。
- **dagq::it の 20%（前の 2,073.7 秒の 20% は約 415 秒）と test 段の約 1 分には届かなかった。** 変えた module の減りは dagq::it の 4.8%（−98.8 秒、6 並列で約 16 秒）、load の近い log で 6.4%（約 23 秒）、変えていない共通の test の伸び（+17.4%）で load と揺れを補正した観測でも約 8.4%（約 −174 秒、約 29 秒）。
- **変えた module の flaky と失敗は増えていない。** 前 20 本の flaky 6 件（plan_review 2・runtime_stall 2・lifecycle_cmux 1・runtime_host_metrics 1）と失敗 1 件（queue_service_reads）、後 20 本の flaky 1 件（runtime_adopt）と失敗 0 件。どれも変えた module ではない。
- **goal 68 の受け入れ条件 (4) の「同じ方法の本番の coverage の関門の log の前後で比べた結果が docs/plans にある」はこの文書が満たす。** 目安の 20% には届かないので、次に判断を unit test へ移す候補を 6 章に書いた。load と別の着地が重なるので、上の差は観測であり、5 つの task の因果効果とは言い切らない。

## 1. 手順

1. 集計の script は [measure.py](integration-to-unit-tests/measure.py)（Python の標準ライブラリだけ）。repository の root で次を打つ。

   ```sh
   python3 docs/plans/integration-to-unit-tests/measure.py \
     --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
     --metrics ~/.local/share/dagq-hostmetrics/metrics.csv \
     --out-dir docs/plans/integration-to-unit-tests
   ```

   `--runs-dir` と `--metrics` は既定値を持たない必須の引数。script は queue を開かず、`dagq` を呼ばず、`--runs-dir` の下に書かない（`dagq locate` はクライアントモードで runs_dir を返さないので使わない）。読むのは log と metrics.csv と worktree の `git log` だけで、書くのは `--out-dir` の [logs.csv](integration-to-unit-tests/logs.csv)（選んだ log の相対 path・mtime・区間・並列数・件数・秒・load）と [excluded.csv](integration-to-unit-tests/excluded.csv)（除いた log と理由）。集計は標準出力に JSON の行で出す。
2. `--runs-dir` の `*/integrate-*-verify-*.log` のうち `Summary [` を含むもの（coverage の関門の nextest）を mtime で区切る。採る範囲は依頼の区間の始め（2026-10-02 18:20:30 JST）の 1 日前から、この run の base `0045125d` の commit 時刻（2026-10-03 18:25:05 JST）まで。
3. ANSI の色を除き、test ごとの最終結果の行を採る。`PASS [ 秒s]`、今の nextest の `FLKY-FL n/m [ 秒s]`（`.config/nextest.toml` の `retries = 1` と `flaky-result = "fail"` の下で流し直して通った test。Summary の後に出る）、古い nextest の `FLAKY n/m [ 秒s]`。FLKY-FL と FLAKY の秒は通った回の時間（[slow-tests](../design/slow-tests.md) と `scripts/slow-tests.sh` と同じ扱い）。`TRY n PASS`・`TRY n FAIL`・`SLOW`・`SKIP` と、流し直しでも落ちた最終の失敗（Summary の後に `TRY n FAIL` か `FAIL` で並ぶ行）は時間に足さない。flaky の件数は FLKY-FL と FLAKY の合計、失敗の件数は Summary の後の最終の失敗の test の数。
4. 照合: log ごとに、採った test の数（同じ test は 1 回）＋最終の失敗の数が Summary の `N tests run` と一致し、PASS の数が Summary の `passed` から FLAKY の数を引いた数（今の形式では `passed` そのもの。FLKY-FL は Summary の failed に数えられる）と一致することを確かめ、合わない log は理由つきで除く。1059 の measure.py の `assert len(records) == len(tests) == passed` は FLKY-FL を数えられないので踏襲していない。
5. load は metrics.csv（約 30 秒ごと、ts は JST）の load1 の、mtime−Summary〜mtime の平均と最大。coverage の report の時間も mtime に含まれるので、test 段より少し後ろにずれる（[slow-test-waits](slow-test-waits.md) 1 章と同じ近似）。
6. log ごとの合計の中央値と、test ごとに log 間の中央値を取ってから足した値は別々に出す。module は binary `dagq::it` の test 名の最初の `::` の前で束ねる。test の追加・削除の影響を見るため、両区間に共通の test だけの合計も出す。

### 照合の結果

区間の候補の 128 本は全て一致した。除いたのは 1 本だけで、`5328601b-11ed-451e-b732-f4b38036d4d4/integrate-3-verify-3.log`（2026-10-02 14:32:46、Summary が `1937/2395 tests run` の途中で止まった関門）。どの区間にも入らない時刻である。例の `8dcd7775-f024-41e5-931b-153b51f134e0/integrate-1-verify-3.log` は `2492 tests run: 2490 passed, 2 failed` で、PASS 2,490 本と FLKY-FL 2 本を採り、失敗は 0 本になる。

## 2. 区間

境目は task 1412〜1416 の着地の commit の時刻（worktree の `git log`）。前は最初の着地より前の直近 20 本、後は最後の着地より後の最初の 20 本。時刻は JST。

| task | 内容 | 着地の commit | 着地の時刻 |
| --- | --- | --- | --- |
| 1414 | runtime_review・runtime_review_concern | `9d74de58` | 10-03 02:33:17 |
| 1412 | runtime_resume | `7fea2d6b` | 10-03 03:24:54 |
| 1413 | runtime_integrate | `ead1223c` | 10-03 03:34:13 |
| 1416 | parked_conflict()・awaiting_run() の fixture | `05000a7a` | 10-03 04:24:03 |
| 1415 | runtime_job_verdicts・runtime_triage | `a9397a71` | 10-03 04:54:24 |

並列数は log に出ない。`dagq.toml` の `NEXTEST_TEST_THREADS` の値は `40f693f2`（2026-09-29 05:26:39）で 8→6 になり、以後この区間の終わりまで変わっていない（`git log -G'NEXTEST_TEST_THREADS *= *"' -- dagq.toml`。`051fda86` はコメントだけ）。どの区間も全て 6 並列と推定した（logs.csv の `threads_inferred`）。

| 区間 | 本数 | 期間（mtime） | 含む着地 |
| --- | --- | --- | --- |
| 依頼のとき（参考） | 20 | 10-02 18:20:30〜22:29:41 | 5 つとも前。前の区間と 8 本が重なる |
| 前（主な比較） | 20 | 10-02 20:19:48〜10-03 02:33:16 | 5 つとも前。最後の 1 本は 1414 自身の関門（1414 の変更を含む） |
| 着地の途中（参考） | 7 | 10-03 02:51:44〜04:54:23 | 1414 だけ後が 2 本、1412・1413 まで後が 3 本、1416 まで後が 2 本。最後の 1 本は 1415 自身の関門 |
| 後（主な比較） | 20 | 10-03 05:20:11〜12:30:13 | 5 つとも後 |

前の 20 本の mtime の間に着地した commit は 24 本（1414 を含まない）、後の 20 本の間は 30 本（`22d0493f` 〜 `9edb8fd8`。どちらもその着地自身の関門の log が区間に入る）。後の区間には test を足した着地（`22d0493f` の runtime_cleanup、`036871ca` の runtime_background*、`e850ad3a` の runtime_recheck、`8b612b3a` の planner_headless など）が重なり、着地の途中の区間に着地した `91f1bc3f`・`acc7d12e` の review_subagents の test も後の区間から出る。前の区間の最後の 1 本（1414 自身の関門）を除いた 19 本でも、合計の中央値 2,197.9 秒・Summary 376.0 秒・変えた module 429.4 秒で、比べ方は変わらない。

## 3. 前後の比較

### 全体

| 指標（秒。load 以外は中央値） | 依頼のとき | 前 | 着地の途中 | 後 |
| --- | --- | --- | --- | --- |
| 本数 | 20 | 20 | 7 | 20 |
| 並列数 | 6 | 6 | 6 | 6 |
| log ごとの test の時間の合計 | 2,138.3（1,683.6〜2,495.0） | 2,205.3（1,804.8〜2,623.8） | 2,086.0 | 2,533.4（2,055.4〜3,456.4） |
| test 段（Summary） | 366.4（290.6〜469.8） | 376.8（310.8〜445.5） | 357.7 | 440.3（352.3〜584.1） |
| test ごとの中央値の合計（出現した全 test） | 2,031.2（2,478 本） | 2,202.1（2,518 本） | 2,130.6 | 2,643.6（2,672 本） |
| dagq::it の本数・test ごとの中央値の合計 | 1,063 本・1,912.1 | 1,084 本・2,073.7 | 1,097 本・2,008.9 | 1,114 本・2,492.9 |
| dagq（lib）の本数・合計 | 1,238 本・68.3 | 1,257 本・74.8 | 1,307 本・71.8 | 1,381 本・91.1 |
| 変えた 6 module の log ごとの合計 | 415.7 | 430.3 | 321.0 | 333.6 |
| dagq::it のうち変えた 6 module の割合（log ごと） | 20.8% | 20.7% | 16.0% | 13.6% |
| dagq::it の残りの log ごとの合計 | 1,566.9 | 1,637.8 | 1,670.4 | 2,067.0 |
| load1 の test 段の平均の中央値（範囲） | 12.18（6.30〜17.48） | 14.26（6.30〜21.78） | 14.17（8.38〜21.86） | 17.96（10.91〜28.77） |
| load1 の test 段の最大の中央値（範囲） | 16.23（7.23〜26.37） | 18.66（7.23〜33.16） | 18.81（10.19〜26.89） | 21.20（12.89〜37.36） |
| flaky（FLKY-FL＋FLAKY）・その log 数 | 3・3 | 6・5 | 0・0 | 1・1 |
| 失敗・その log 数 | 6・2 | 1・1 | 0・0 | 0・0 |

依頼のときの値（PASS と FLAKY だけで採った test 段 366 秒・合計 2,136 秒・dagq::it 1,063 本 1,912 秒・lib 1,238 本 68 秒・load 12.2 / 16.2）は同じ 20 本で再現した。FLKY-FL も採ると、log ごとの合計の中央値が 2,136.1→2,138.3 秒、dagq::it の残りの log ごとの中央値が 1,563.8→1,566.9 秒になる（FLKY-FL のある 2 本の分）。ほかの値は変わらない。依頼のときの dagq::it の 1,063 本は 20 本の間に出た test の和で、後の区間の 1,114 本と同じ数え方である。

### 変えた module ごと（test ごとの中央値の合計、秒）

| module | 依頼のとき | 前 | 後 | 差（前→後） | load の近い前 8 本 | load の近い後 7 本 |
| --- | --- | --- | --- | --- | --- | --- |
| runtime_resume | 30 本・144.5 | 30 本・147.3 | 21 本・119.2 | −28.1（−19%） | 151.3 | 114.1 |
| runtime_integrate | 35 本・99.3 | 35 本・100.7 | 23 本・73.3 | −27.4（−27%） | 104.6 | 67.6 |
| runtime_review | 18 本・61.9 | 19 本・65.1 | 14 本・40.8 | −24.3（−37%） | 67.8 | 38.2 |
| runtime_review_concern | 9 本・21.2 | 10 本・21.4 | 9 本・18.3 | −3.1 | 19.9 | 15.6 |
| runtime_job_verdicts | 9 本・43.8 | 9 本・46.1 | 9 本・23.7 | −22.4（−49%） | 48.1 | 21.1 |
| runtime_triage | 24 本・47.6 | 24 本・51.7 | 24 本・58.2 | +6.5（+13%） | 53.6 | 52.7 |
| 6 module の計 | 125 本・418.3 | 127 本・432.3 | 100 本・333.5 | −98.8（−22.9%） | 445.3 | 309.3（−30.5%） |

本数は区間の中で一度でも出た test の数（前の runtime_review の 19 本・runtime_review_concern の 10 本は、前の最後の 1 本（1414 自身の関門）にだけ出る test を含む。これを除いた 19 本では 18 本と 9 本。後の runtime_review_concern は 1414 の後の着地（`acc7d12e`・`bfe4edf7`）が 3 本足して 9 本）。runtime_triage は 1415 が 2 本の supervise を 1 回ずつ減らしただけで、本番では減りより伸びの方が大きく出た（load の伸びと重なる）。

### fixture を使う上位の module と、ほかの群（test ごとの中央値の合計、秒）

| 群 | 前 | 後 | うち両区間に共通の test（本数、前→後） |
| --- | --- | --- | --- |
| `runtime_resume*` | 43 本・212.9 | 34 本・199.0 | 33 本、167.5→192.2 |
| `runtime_review*` | 78 本・202.7 | 72 本・194.2 | 69 本、182.3→187.7 |
| `runtime_handoff*`（parked_conflict を 7 回呼ぶ） | 21 本・71.6 | 24 本・90.3 | 21 本、71.6→82.8 |
| `runtime_waiting*` | 16 本・73.7 | 16 本・85.4 | 16 本、73.7→85.4 |
| `runtime_headless*` | 33 本・87.6 | 33 本・103.5 | 33 本、87.6→103.5 |
| `runtime_session*` | 30 本・56.0 | 30 本・67.1 | 30 本、56.0→67.1 |
| `runtime_landing*` | 15 本・63.0 | 15 本・73.7 | 15 本、63.0→73.7 |
| `runtime_stall*` | 21 本・60.0 | 21 本・70.0 | 21 本、60.0→70.0 |
| `runtime_adopt*` | 19 本・34.3 | 19 本・43.9 | 19 本、34.3→43.9 |
| `cli_*` | 108 本・95.6 | 110 本・109.9 | 108 本、95.6→109.6 |

### 両区間に共通の test だけ

| 範囲 | 本数 | 前 | 後 |
| --- | --- | --- | --- |
| 全 binary | 2,484 | 2,094.2 | 2,380.6（+13.7%） |
| dagq::it | 1,050 | 1,965.8 | 2,242.6（+14.1%） |
| うち変えた 6 module | 93 | 324.5 | 314.9（−3.0%） |
| うち変えていない module | 957 | 1,641.3 | 1,927.7（+17.4%） |
| dagq（lib） | 1,257 | 74.8 | 78.4 |

変えていない共通の test は 17.4% 伸びた。後の load の高さと、同時に走る test の増え方による伸びと読む。変えた module の共通の test（残した境界の test）は −3.0% で、変えていない test のように伸びなかった。縮めた fixture と supervisor の起動の分が伸びを打ち消したと読むが、分けては測っていない。

### 消えた test と足された test（dagq::it と lib、test ごとの中央値）

- 前だけに出た test は 34 本・107.9 秒（runtime_integrate 14 本・39.8 秒、runtime_resume 10 本・45.5 秒、runtime_review 5 本・10.8 秒、runtime_review_concern 4 本・9.5 秒、runtime_triage 1 本・2.3 秒。名前を変えた test を含む）。
- 後だけに出た test は 188 本・263.0 秒。lib 124 本・12.6 秒のほかは、1412〜1415 が名前を変えた・足した約 4 本（runtime_integrate 2 本・runtime_resume 1 本・runtime_triage 1 本、約 12 秒）と、review_subagents 17 本・53.2 秒、runtime_cleanup 3 本・51.5 秒、runtime_recheck 8 本・33.1 秒、runtime_background_process 5 本・32.5 秒、planner_headless 5 本・27.1 秒、runtime_background 11 本・26.5 秒など、別の着地が足した integration test。
- task 1412〜1415 が判断を移した src の module（`application::supervise::{resume,deliver,landing,recovery,triage}::`・`domain::{resume,concern,recovery}::`・`application::{integrate,prompt}::`）で新しく出た unit test は 42 本・合計 0.74 秒。

### load の近い log どうし

後の load が高いので、load1 の平均が近い log どうしでも比べた。

| 範囲 | 本数（前 / 後） | load 平均の中央値（前 / 後） | 合計の中央値 | Summary の中央値 | 変えた 6 module（log ごと） | dagq::it の残り（log ごと） |
| --- | --- | --- | --- | --- | --- | --- |
| load 12〜17 | 8 / 7 | 15.66 / 15.09 | 2,205.3→2,311.7 | 376.8→393.8 | 450.6→313.7 | 1,658.8→1,880.4 |
| load 8〜16 | 10 / 8 | 12.14 / 15.04 | 2,136.1→2,294.7 | 365.6→391.6 | 424.4→304.6 | 1,560.2→1,874.7 |
| load 10〜20 | 12 / 14 | 14.26 / 15.69 | 2,205.3→2,451.1 | 376.8→419.2 | 450.6→317.0 | 1,637.8→1,958.1 |

load の近い組でも、変えた module の −120〜−137 秒を、変えていない module と新しい test の +220〜+320 秒が上回った。load の平均が同じでも、同時に走る build・e2e・worker の状態は同じではない。後の区間の 21 本目以降（〜10-03 18:24、13 本）は load の中央値がさらに高く（後 33 本の全体で 21.8）、比べる標本には入れていない。

### 目安との比べ

| 指標 | 値 | 目安 |
| --- | --- | --- |
| 変えた module の減り（test ごとの中央値の合計、前→後） | −98.8 秒、前の dagq::it 2,073.7 秒の 4.8%、6 並列で約 16 秒 | dagq::it の 20%（約 415 秒）、test 段の約 1 分 |
| 同、load の近い log（12〜17） | −136.0 秒、load の近い前 8 本の dagq::it 2,119.4 秒の 6.4%、約 23 秒 | 同上 |
| 同、変えていない共通の test の伸び（+17.4%）で補正した観測 | 432.3×1.174＝507.5 秒の見込みに対して 333.5 秒、−174 秒、8.4%、約 29 秒 | 同上 |
| 全体（log ごとの合計 / Summary） | +328.1 秒 / +63.5 秒（load の近い組で +106.4 / +17.0 秒） | 縮むこと |

どの見方でも目安に届いていない。本番の全体の合計と Summary は縮まず、goal 68 の受け入れ条件 (2) の「縮んだことが数字で示される」をこの区間の全体の値では示せない。変えた module の範囲では、worker の A/B（4 章）と同じ向きに縮んだ。

## 4. 先行 task の行き先と A/B

各 task の receipt の summary（この run の prompt に載った predecessor summary）からまとめた。移した・減らした test ごとの行き先の表の全文は各 task の receipt にある。ここでは module ごとに要約する。

### 移した・減らした test の行き先

| task | 減らした・縮めた test（例） | 行き先 |
| --- | --- | --- |
| 1412 runtime_resume | skip の 8 本（parked のまま・人の差し戻し・古い head・main の先行・別の run の receipt・failed・evidence 欠け） | `supervise::resume::tests::a_run_is_skipped_only_when_every_condition_of_the_skip_holds`（各 case）と既存の `history::tests`。dirty の worktree の 1 本は境界として残し、開けない resume の 1 試行目の確認をここへ移した |
| 1412 | conflict_only・three attempts・kill の数え方 | `domain::resume::tests::{a_counted_resume_starts_with_no_repair, a_conflict_only_resume_is_an_uncounted_repair, a_kill_only_resume_is_an_uncounted_repair}` と `resumed_text_counts_every_kind_of_resume`。end-to-end は最後の 1 試行だけ session で流す形に縮めて残した |
| 1412 | 依頼の送り直し | `deliver::tests::a_lost_text_is_sent_again_only_once` と、縮めた `a_request_lost_twice_is_asked_to_the_inbox` |
| 1413 runtime_integrate | push の結果（拒否・remote の確認の失敗・no_push・origin 無し・repository の表） | `application::integrate` の unit test（`decide_push` と fake の remote）。実 push の境界（remote 名・attention・2 commit の push）は残した |
| 1413 | migration の振り直し（source の外・参照・2 本・rename） | `plan_renumber` の unit test と `git_adapter_reads_what_the_renumbering_plan_needs`（tempdir と git だけ）。拒否の commit の巻き戻しと再着地は残した |
| 1413 | verify の失敗の分類・held・test 名 | `judge_command`・`verification_failed()` の unit test。event への配線は `failing_verification_command_passes_validation_and_needs_a_session_at_integrate` に寄せた |
| 1413 | prompt（goal・context・siblings）と integrate の log 名 | `prompt.rs` と `integrate.rs` の unit test（MemoryFiles）。claim の時点の配線は残した |
| 1414 runtime_review・concern | 失敗した review の 4 case・unreadable・timeout・span・起動できない review | `landing::tests`（`retries_review`・`failed_review_question`・`landing_action` など 9 本）と、exit 3 の 1 case・span の 1 case に縮めた integration test |
| 1414 | concern の推奨・low/scope/discard・推奨なし・上限の send_back・引き継ぎ | `landing::tests` と `concern::tests`。discard・high の引き継ぎ・上限の配線は 1 case ずつ残した |
| 1415 runtime_job_verdicts | 壊れた verdict の loop 4 本（no JSON・欠けた欄・未知の欄・未知の variant） | `domain::recovery::tests::a_broken_verdict_is_refused_with_what_is_wrong`、`supervise::recovery::tests` の 3 本。alert ごとに代表の 1 case を残した |
| 1415 runtime_triage | round の使い切り・decide の ask の文面 | `supervise::triage::tests` の 3 本。end-to-end は `a_corrected_verify_gets_a_round_past_the_used_up_limit_and_lands_inherited` に寄せた |
| 1416 fixture | （test は減らしていない） | `warm_dagq()` で最初の dagq の exec の待ちを fixture の準備と重ねた |

### 1583 で変えた headless の境界 test の stress

対象は `runtime_headless::a_turn_past_its_limit_is_stopped_with_its_command_outside_its_group`。1583 の receipt（event 82032）の行き先の表は、削除した `a_silent_turn_is_stopped_and_its_recovery_job_resumes_the_session` の recovery・同じ session の resume・着地の確認をこの境界 test に移した。process group 外の子孫を `ps` で確かめるため、1583 の worker は sandbox の手元で stress を流せず、ask 395 で host に移した。その手順を持った follow-up の task 1659 は、手順の script が消えたため取り消された。

行き先の resume の誤停止の調査・修正と負荷の下の stress は task 1629（goal 37）が持つ。その着地 commit は `79a47c1a50ae08346ce8ce3ce4708d5a78fa39d3`（`git log -1 79a47c1a50ae08346ce8ce3ce4708d5a78fa39d3` で確認）。この commit は resume の ready の後、2 秒の時計の中に残っていた `git rev-parse HEAD` を ready の前へ移した。task 1785 は src・tests を変えず、この着地の後の retries なしの 5 周 stress の確認だけを持つ。

ps を実行できる host の `integrate` が rebase 後の verify の 2 本目で次を流す。前後の `uptime` で load average を残し、test の終了コードを保つ。

```sh
sh -c 'git rev-parse HEAD; uptime; cargo nextest run --locked --test it --retries 0 --stress-count 5 -E "test(=runtime_headless::a_turn_past_its_limit_is_stopped_with_its_command_outside_its_group)"; s=$?; uptime; exit $s'
```

`--retries 0` はこの stress に限る。通常の `.config/nextest.toml` は `retries = 1` で、落ちた test が全て FLAKY なら integrate は worker を resume せず検証をやり直し、その回は `NEXTEST_FLAKY_RESULT=pass` で FLAKY も成功と数える（[integrate の「不安定なtestの着地のやり直し」](../design/supervisor-lifecycle/integrate.md)、ADR-t768-1・ADR-t1039-1）。この救済で 5/5 を満たしたことにしないため、再試行を無効にする。1 周でも test が落ちれば FLAKY ではなく `test_failure` として着地を止め、`needs_session` にする。

証拠は `runs/<run>/integrate-<attempt>-verify-2.log`（一般形は `runs/<run>/integrate-*-verify-*.log`）と `verification_command` event に残す。log の commit・コマンド・5 周の結果・前後の load average を対応づけ、goal review は `dagq events --full --task 1785 --kind verification_command` で着地した run の stress のコマンドが `exit_code: 0`、`flaky_tests` が空であることを読む。合格は FLAKY を含まない 5/5 passed と exit 0。worker は ps の要らない文書の照合と手元の静的検査だけを行い、integrate 前に stress の結果を推測で書かない。

integrate の stress が落ちて resume された worker は src・tests を直さず、落ちた周・test・load average・log の場所を failed receipt に書く。原因の調査・修正を持つ follow-up（goal 37 に合い、同じ retries なしの 5 周 stress を verify に持つ）を提案する。修正の着地の後に task 1785 を流し直し、その integrate で 5/5 passed になって初めて着地する。以前の run・試行に失敗があれば、goal review は同じ events と修正 task の receipt で、failed receipt が提案した修正の着地と、その後の合格を対応づける。

### 外部プロセス・固定の待ち・fixture の数と module の A/B（各 task の worker が手元で測った値）

| task | 数（前→後） | 固定の待ち（前→後） | A/B：test の時間の合計の中央値（前→後） | A/B：Summary の中央値 |
| --- | --- | --- | --- | --- |
| 1412 | test 30→21、fixture() 30→21、parked_conflict() 28→19、supervise 系 約 35→26、resume の session の起動 約 20→4 | 約 10.7→3.9 秒 | 140.9→93.7 秒（−33%） | 26.1→17.3 秒 |
| 1413 | test 35→23、awaiting_run() 20→12、fixture() 15→11、supervise 19→13、run_agent 4→2、fake の remote での integrate 7→2、bare の remote 6→5、実 git push 6→6 | 0→0 | 101.0→53.8 秒（−47%）、revise で再着地を足して 70.2 秒（−30%） | 19.3→9.7 秒（revise 後 12.9 秒） |
| 1414 | runtime_review: test 18→14、fixture 27→14、supervisor の起動 38→19、integrate 2→1、loop の中の起動 22→0。runtime_review_concern: test 9→6、fixture 12→6、起動 12→6、loop 5→0 | review の timeout 2→0 秒、stub の sleep 2.0→0.5 秒 | 77.9→44.6 秒（−43%） | 13.7→7.9 秒 |
| 1415 | runtime_job_verdicts: fixture の実行 21→10、supervisor の起動 21→10、loop の中 15→0、stub の session 4→1。runtime_triage: 起動 31→29 | job_verdicts 2.0→0.5 秒、triage 5.5→5.5 秒 | 100.9→72.3 秒（−28%） | 18.0→13.1 秒 |
| 1416 | fixture の 1 回: awaiting_run 1,202.5→1,022.5 ms（−15%）、parked_conflict 1,639.5→1,474 ms（−10%） | 0→0 | module の水準では load の揺れ（±20%）の中で判定できない | 同左 |

A/B は対象 module だけを手元で 3 回ずつ流した値で、本番の関門（instrument あり・全 test・load 15〜20）とは標本も精度も違うので足さない。本番の変えた module の減り（−22.9%、load の近い組で −30.5%）は、A/B の −28〜−47% の下の端か、それより小さい。本番では残した境界の test が load の伸びを受けたためと読む。

## 5. 読み方の注意

- **因果とは言い切らない。** 後の区間は load が高く（平均の中央値 14.26→17.96）、30 本の別の着地（docs と measure を含む）があり、その一部と着地の途中の区間の着地が test を足し、supervisor の振る舞い（cleanup・handoff・review の subagent など）も変えた。全体の伸びを 5 つの task のせいとも、変えた module の減りを 5 つの task だけの効果とも言わない。
- **着地の直前の関門には、その着地の変更が入る。** 前の最後の 1 本は 1414 自身の関門で、除いても値の向きは変わらない（2 章）。
- **本数は区間の中で一度でも出た test の数。** 区間の途中で足された test、名前を変えた test は前後の本数に両方現れることがある。
- 並列数は `dagq.toml` の履歴からの推定で、どの区間も 6。test の時間の合計と Summary の比（後の中央値 2,533.4÷440.3＝5.75）は 6 並列に近い。

## 6. 次に判断を unit test へ移す候補

目安の 20%（後の dagq::it 2,492.9 秒に対して約 500 秒）に届いていないので、後の区間の module ごとの合計（test ごとの中央値の合計、秒）から候補を選んだ。見積もりは、本番で変えた module に起きた減り（−22.9%、load の近い組で −30.5%）を当てはめた幅である。

| 候補 | 後の本数・合計 | 理由 | 見積もり（test の時間の合計 / 6 並列の test 段） |
| --- | --- | --- | --- |
| runtime_cleanup | 13 本・109.6 | module の合計では resume の次に重い（1 本あたり約 8.4 秒）。53.5→109.6 秒（3 本を足した） | 25〜33 / 4〜5.5 |
| `runtime_headless*` | 33 本・103.5 | turn の再開・沈黙の判定を case ごとに supervisor で確かめる | 24〜32 / 4〜5 |
| runtime_handoff | 24 本・90.3 | parked_conflict を 7 回呼ぶ。引き継ぎの判断と境界が混ざる | 21〜28 / 3.5〜4.5 |
| `runtime_waiting*`（waiting_stages・waiting） | 16 本・85.4 | 1 本あたり約 5.3 秒。待ちの段の判断を時刻を値で渡して確かめられる | 20〜26 / 3〜4.5 |
| runtime_session | 30 本・67.1 | send・input box の判断 | 15〜20 / 2.5〜3.5 |
| runtime_landing_release | 11 本・54.0 | 1 本あたり約 4.9 秒。着地の解放の判断 | 12〜16 / 2〜3 |
| runtime_recheck・review_subagents（後で足された） | 12 本・55.8、17 本・53.2 | 足された時から integration test が多い | 25〜33 / 4〜5.5 |
| runtime_resume の残り（`runtime_resume*`） | 34 本・199.0 | 1412 の後も最大の群。resume_exit_retry・resume_adopt が残る | 46〜61 / 7.5〜10 |

合計で約 190〜250 秒（6 並列で約 30〜42 秒）の見込みで、ここまで移しても 20% の約 500 秒には届かない。fixture の実時間（`SqliteQueue::init` の migration と、最初の dagq の exec の XprotectService の scan）と load の伸びを減らす別の手当て（[slow-test-waits](slow-test-waits.md) の 3 章・task 1416 の計測）を合わせて検討する。候補の登録は follow_up（improvement）として planner に渡す。後の区間 20 本の同じ手順での測り直しは要らない（そろっている）。
