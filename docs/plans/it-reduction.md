---
id: plan-it-reduction
type: plan
title: tests/it の全 1,192 本の分類（境界・判断・代表あり・goal 92 で消える）と、it でないと担保できない test の見積もり
status: active
created: 2026-10-04
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - plan-integration-to-unit-tests
  - plan-slow-test-waits
  - design-slow-tests
---

# tests/it の全 1,192 本の分類（境界・判断・代表あり・goal 92 で消える）と、it でないと担保できない test の見積もり

inbox が 2026-10-04 に subagent で `tests/it` の全 1,192 本を分類した明細を repository に置き、集計と読み方を書く（goal 118、task 1706）。後続の task（関門の許可の一覧の理由、移し替えの対象の一覧、前後の測定）はこの文書と 2 つの TSV を起点に読む。src/ と tests/ は変えていない。

## 要点

- **it でないと品質を担保できない（境界の B）のは 364 本・826.1 秒で、本数の 30.5%・秒の 28.6%。** 分類の揺れ（B/R/D の境目 ±1 割）を見込むと 328〜400 本・743〜909 秒。
- **goal 92 で消える G（230 本・667.5 秒）を除いた後の it（962 本・2,217.0 秒）では、B は本数の 37.8%・秒の 37.3%**（揺れを見込むと 34〜42%）。残りの 6 割強（D 492 本・1,076.2 秒と R 106 本・314.7 秒）は、判断を src の unit test に移せば消せるか unit に移せる。
- goal 119（task 1709〜1713）の着地の前後の本番の関門の比較は「[goal 119 の前後](#goal-119-の前後)」（task 1714。後の区間が 1 本しかない暫定）。
- D+R の秒の大きい上位 20 module で D+R の秒の 59%（823.9 / 1,390.6 秒。module ごとの値なので modules.tsv の秒）を占める。最大は `runtime_cleanup`（152.4 秒）。

## 明細

- [it-tests.tsv](it-reduction/it-tests.tsv): 1 本ごと。列は `module`・`test`・`class`・`secs`・`reason`。1,193 行（見出し 1 行＋1,192 本）。`module::test` の重複は無い。
- [modules.tsv](it-reduction/modules.tsv): module ごとの集計。列は `module`・`B`・`D`・`R`・`G`（本数）と `B_secs`・`D_secs`・`R_secs`・`G_secs`（秒）。163 行（見出し 1 行＋162 module）。

どちらも host の `~/.local/share/dagq-hostmetrics/inbox/it-classification-2026-10-04/` のファイルをそのまま写した（`cmp` で同じ中身を確かめた）。

## (a) 分類の定義

| class | 名前 | 定義 |
| --- | --- | --- |
| B | 境界 | 境界（プロセス・ファイル・Git・CLI・DB・時間の経過など）を通して確かめるもので、it でないと担保できない |
| D | 判断 | 確かめているのが判断で、src の副作用のない関数に切り出して unit test に移せる |
| R | 代表あり | 境界に触れるが、同じ module の別の 1 本がその境界を代表して確かめており、判断を unit に移せば消せる |
| G | goal 92 で消える | goal 92 の対話・cmux の廃止で、確かめている経路ごと無くなる |

境界と判断の分け方は [testing.md](../development/testing.md) の「判断と境界のtest」（ADR-t1410-1）に従う。

## (b) 本数・秒・割合

| class | 本数 | 秒 | 本数の割合 | 秒の割合 |
| --- | ---: | ---: | ---: | ---: |
| B 境界 | 364 | 826.1 | 30.5% | 28.6% |
| D 判断 | 492 | 1,076.2 | 41.3% | 37.3% |
| R 代表あり | 106 | 314.7 | 8.9% | 10.9% |
| G goal 92 で消える | 230 | 667.5 | 19.3% | 23.1% |
| 合計 | 1,192 | 2,884.5 | 100% | 100% |

`it-tests.tsv` から計算した（repository の root で打つ）。

```sh
awk -F'\t' 'NR>1{n[$3]++;s[$3]+=$4;N++;S+=$4}
  END{for(c in n)printf "%s %d %.1f %.1f%% %.1f%%\n",c,n[c],s[c],100*n[c]/N,100*s[c]/S;
      printf "total %d %.1f\n",N,S}' docs/plans/it-reduction/it-tests.tsv
```

依頼のときの値（B 364 本・826 秒・29%、D 492・1,076・37%、R 106・315・11%、G 230・668・23%）と照らすと、本数と秒は一致し、割合は **秒の割合** だった（29%・37%・11%・23% は上の表の秒の割合を丸めたもの）。本数の割合は 30.5%・41.3%・8.9%・19.3% で、表には両方を書いた。

`modules.tsv` の列の合計（`awk -F'\t' 'NR>1{for(i=2;i<=9;i++)t[i]+=$i}END{for(i=2;i<=9;i++)printf "%.1f ",t[i];print ""}' docs/plans/it-reduction/modules.tsv`）は本数が 364・492・106・230 で一致し、秒は 826.3・1,075.8・314.8・667.5 と 0.4 秒以内の差がある。module ごとに it-tests.tsv から集計し直すと、本数は 162 module 全てで一致し、秒は `runtime_codex` の B（10.3 と 10.4）・`runtime_integrate` の B（51.1 と 51.0）・`runtime_recheck` の D（30.1 と 30.0）の 3 か所だけ 0.1 秒違う。modules.tsv が module ごとに小数 1 桁へ丸めた差で、本文の秒は it-tests.tsv から計算した値を使い、module ごとの値（(f) の表と、そこから足した 823.9・1,390.6・162.1 秒）だけ modules.tsv の値を使う。

## (c) 秒の出所

`secs` は本番の integrate の coverage の関門（`cargo llvm-cov nextest`）の直近 10 本の log の、test ごとの時間の中央値。inbox が補足した同じ 10 本の値は、test 段 2,875 本、Summary 400〜640 秒、log ごとの test の時間の合計の中央値 3,194 秒のうち `dagq::it` が 1,186 本・3,029 秒。

- 明細の 1,192 本は分類の時点の `tests/it` の本数で、10 本の log の `dagq::it` の 1,186 本とは 6 本違う（10 本の間に足された・名前を変えた test）。
- 明細の合計 2,884.5 秒は **test ごとの中央値の合計**、3,029 秒は **log ごとの合計の中央値** で、別の統計量なので一致しない。割合と順位を読むのに使い、関門の Summary（6 並列の壁時計）の秒とは読み替えない。

## (d) 判定の方法と揺れ

- 判定は test の名前・doc comment・fixture の印（使う stub・supervisor の起動・cmux の fixture など）から行い、本文の全部は読んでいない。`reason` 列はその根拠の短い要約。
- G は対話・cmux・in-cmux の印で機械的に判定したので揺れは小さい（`reason` の多いものは cmux screen の idle の推定・interactive の exit の retry・`/exit` の送り方など）。
- B/R/D の境目には **±1 割の揺れ** を見込む。境界に見えて判断だけを確かめている test や、その逆があり得る。
- **task を切るときは対象の test の本文で確かめ直す。** この TSV の class は候補の目安で、移す・消すの根拠そのものにはしない。

## (e) it でないと品質を担保できないものはどれくらいか

| 範囲 | B の本数 | B の秒 | 本数の割合 | 秒の割合 |
| --- | ---: | ---: | ---: | ---: |
| 今の it の全体（1,192 本・2,884.5 秒） | 364（揺れ 328〜400） | 826.1（揺れ 743〜909） | 30.5%（27〜34%） | 28.6%（26〜32%） |
| G を除いた後（962 本・2,217.0 秒） | 364（揺れ 328〜400） | 826.1（揺れ 743〜909） | 37.8%（34〜42%） | 37.3%（34〜41%） |

揺れの幅は B の本数と秒の ±1 割（境目で B と R/D の間を行き来する分）で、合計は変えずに B の割合だけを動かして計算した。

答え: **it でないと担保できないのは今の it の約 3 割（364 本・約 830 秒）、goal 92 の後の it では約 4 割弱。** 判断の移し替え（D と R）が全部済めば、it は B の 364 本前後・約 830 秒前後まで縮み得る。ただし移した判断の unit test と、境界を代表する it が 1 本ずつ残ることが前提で、D と R の全部が消えるわけではない（D は unit test として残る）。

## (f) D+R の大きい module の上位 20 と受け持ち

`modules.tsv` の D+R の秒の降順（同じ秒は module 名の順）。コマンドは sh・bash・zsh で打つ（fish は `$'\t'` を読めない）。

```sh
tail -n +2 docs/plans/it-reduction/modules.tsv \
  | awk -F'\t' '{printf "%s\t%d\t%.1f\t%d\t%d\t%d\t%.1f\t%d\t%.1f\n",$1,$3+$4,$7+$8,$3,$4,$2,$6,$5,$9}' \
  | sort -t$'\t' -k3,3nr -k1,1 | head -20
```

| 順 | module | D+R 本 | D+R 秒 | D 本 | R 本 | B 本 | B 秒 | G 本 | G 秒 | 受け持ち（D・R） |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 1 | `runtime_cleanup` | 9 | 152.4 | 6 | 3 | 7 | 49.1 | 0 | 0.0 | goal 119 か 120 |
| 2 | `runtime_resume` | 14 | 80.1 | 10 | 4 | 3 | 16.8 | 5 | 32.3 | goal 100 の 1557（resume） |
| 3 | `planner_headless_turns` | 7 | 75.5 | 6 | 1 | 1 | 6.6 | 0 | 0.0 | goal 119 か 120 |
| 4 | `plan_review` | 23 | 53.1 | 22 | 1 | 4 | 7.3 | 3 | 12.0 | goal 119 か 120 |
| 5 | `runtime_review` | 11 | 34.2 | 9 | 2 | 1 | 2.5 | 2 | 7.2 | goal 119 か 120 |
| 6 | `review_subagents` | 11 | 33.4 | 10 | 1 | 7 | 15.6 | 0 | 0.0 | goal 119 か 120 |
| 7 | `runtime_codex` | 13 | 32.2 | 12 | 1 | 3 | 10.4 | 0 | 0.0 | goal 119 か 120 |
| 8 | `runtime_headless` | 11 | 31.8 | 6 | 5 | 6 | 25.4 | 0 | 0.0 | goal 119 か 120 |
| 9 | `runtime_broker` | 13 | 30.8 | 11 | 2 | 4 | 11.8 | 0 | 0.0 | goal 119 か 120 |
| 10 | `runtime_stale_receipt` | 6 | 30.8 | 6 | 0 | 1 | 2.4 | 0 | 0.0 | goal 119 か 120 |
| 11 | `runtime_integrate` | 9 | 30.6 | 8 | 1 | 14 | 51.0 | 0 | 0.0 | goal 119 か 120 |
| 12 | `runtime_handoff` | 8 | 30.4 | 5 | 3 | 6 | 12.0 | 10 | 51.9 | goal 119 か 120 |
| 13 | `runtime_landing_release` | 6 | 30.4 | 6 | 0 | 5 | 22.5 | 0 | 0.0 | goal 119 か 120 |
| 14 | `runtime_recheck` | 7 | 30.0 | 7 | 0 | 3 | 16.4 | 2 | 10.4 | goal 119 か 120 |
| 15 | `runtime_provider_switch` | 10 | 29.4 | 9 | 1 | 2 | 5.2 | 1 | 4.6 | goal 119 か 120 |
| 16 | `runtime_waiting_stages` | 2 | 27.9 | 1 | 1 | 1 | 10.9 | 1 | 4.7 | goal 119 か 120 |
| 17 | `runtime_review_questions` | 7 | 24.2 | 7 | 0 | 0 | 0.0 | 1 | 3.8 | goal 119 か 120 |
| 18 | `runtime_e2e` | 4 | 23.1 | 3 | 1 | 2 | 9.9 | 0 | 0.0 | goal 119 か 120 |
| 19 | `runtime_triage` | 14 | 22.4 | 12 | 2 | 6 | 15.4 | 5 | 16.0 | goal 119 か 120 |
| 20 | `inbox_nudge` | 4 | 21.2 | 4 | 0 | 3 | 7.9 | 0 | 0.0 | goal 119 か 120 |

受け持ちは task 1706 の submit 時点（2026-10-04）の queue の goal と task の ID で書く。

| 受け持つもの | goal・task | 範囲 |
| --- | --- | --- |
| 関門と追跡 | goal 118（task 1707・1708） | 足した・変えた it の閾値の関門と、`dagq::it` の本数と時間の日次の追跡。移し替えはしない |
| すぐ着手の移し替え | goal 119（task 1709〜1714） | D・R のうち、goal 100 の 1552・1553 の着地を待たずに移す分 |
| 1552・1553 の後の移し替え | goal 120（task 1715〜1720） | D・R のうち、goal 100 の 1552・1553 の着地の後に移す分 |
| 時間と payload の判断 | goal 100 の task 1557（revise・reopen・resume・session）・1558（stall・stall_recovery・adopt） | resume・revise・reopen・session・stall・adopt の判断（`Instant::now` と payload の読み方）を切り出すので、その module の D・R はここで移す |
| G | goal 92 | 対話・cmux の廃止とともに消える（54 module・230 本・667.5 秒） |

上位 20 の「goal 119 か 120」は、どちらが持つかを各 task の paths が決め、この文書は module ごとに決めない（task 1706 の context は goal の範囲だけを渡し、task の本文は読んでいない）。上位 20 の外の 1557・1558 の module の D+R は `runtime_headless_stall` 5 本・13.8 秒、`runtime_headless_reopen` 3 本・12.5 秒、`runtime_session` 6 本・7.5 秒、`runtime_adopt` 3 本・6.0 秒、`runtime_stall_end` 1 本・0.3 秒。`runtime_stall`・`runtime_stall_recovery`・`runtime_resume_adopt`・`runtime_resume_exit_retry`・`runtime_review_adopt` は D+R が 0 本で、G だけ（合計 42 本・162.1 秒）。

## 周回しない理由

この文書は 2026-10-04 の分類を 1 回だけ読んで集計したもので、周回（定期の取り直し）は要らない。分類は人と subagent の判断で、本番の log から機械的に取り直せるものではなく、移し替えの進み具合は goal 118 の日次の追跡（`dagq::it` の本数と時間の合計）が追う。移し替えの task は対象の test の本文で class を確かめ直すので、この TSV を書き換え続ける必要もない。

## goal 119 の前後

goal 119 の task 1709〜1713 の着地の前後で、本番の integrate の coverage の関門の log から test 段と `dagq::it` を比べる（task 1714、request 20 の人の方針 5）。

**暫定（後の区間は 1 本）。** 最後の着地（task 1710、2026-10-07 10:54）の後に Summary を持つ log は、測った時点（11:18 の着地まで）で 1 本しか無い。20 本の後の区間で同じ手順で測り直すことを follow_up（measurement）に出した。代わりに、1710 を除く 4 本（1709・1711・1712・1713）が着地した後の最初の 20 本（「4 本の後」）を並べる。

- **test 段（Summary）の中央値は 547.0 秒 → 4 本の後 417.4 秒（−129.6 秒・23.7%）、後の 1 本 389.5 秒（−28.8%）。dagq::it は 1,193 本・2,802.0 秒 → 4 本の後 1,038 本・2,213.9 秒（−588.1 秒・21.0%）、後の 1 本 1,073 本・2,133.9 秒（−23.8%）。**
- **このうち移し替えの効果の見積もりは、対象の module で消えた test と足された test の差の、4 本の後で −41 本・−119.4 秒、後の 1 本で −58 本・−151.3 秒（前の dagq::it の 4.3%・5.4%）。** 各 task の A/B の module ごとの中央値の差の和は、着地した 4 task で −88.3 秒、5 task で −121.7 秒（下の表）。A/B は module の時間の変化、こちらは消えた・足された test の秒で、前後の両方にある test の速さの変化を含まないので、同じ量ではない。
- dagq::it の減りの残りの大半は対象の外の module で、4 本の後で消えた 173 本・549.2 秒（goal 92 の対話・cmux の廃止の `runtime_session`・`runtime_handoff`・`runtime_stall_recovery` などと、goal 100 の module）から、足された 59 本・152.6 秒（増え直し）を引いた −396.6 秒。前後の両方にある test の遅さの変化は −72.0 秒。
- 同じ期間に別の着地（goal 92 の task 1437 など）と load の違いが重なるので、どれも観測であり、移し替えが test 段を何秒縮めたかの因果とは言わない。

### 手順

[measure_goal119.py](it-reduction/measure_goal119.py) が読む。Python の標準ライブラリだけで、queue を開かず `dagq` を呼ばず、`--runs-dir` の下に書かない。log の読み方は task 1417 の [measure.py](integration-to-unit-tests/measure.py) を写し、最終の FAIL の取り方だけを直した。

```sh
python3 docs/plans/it-reduction/measure_goal119.py \
  --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
  --metrics ~/.local/share/dagq-hostmetrics/metrics.csv \
            ~/.local/share/dagq/77067154921b9014/host/metrics-2026*.csv \
  --until 2026-10-07T11:19:00+09:00 --out-dir docs/plans/it-reduction/goal-119
```

- `*/integrate-*-verify-*.log` のうち `Summary [` を含むものを mtime で並べ、ANSI の色を除いて PASS・FLKY-FL・FLAKY の行の秒を採る。TRY・SLOW・SKIP と最終の FAIL は時間に足さない。
- 最終の FAIL は、FAIL か TRY n FAIL の行があり PASS・FLKY-FL・FLAKY の行が無い test。今の関門の nextest は最終の失敗を Summary の前に `TRY n FAIL` で出すので、task 1417 の「Summary の後の FAIL」では 22 本の log が合わなかった。
- log ごとに、採った件数＋最終の FAIL の件数が Summary の `N tests run` と一致し、PASS の件数が passed − FLAKY と一致することを確かめ、合わない log は理由つきで除く。
- load は load1 の、mtime − Summary 〜 mtime の平均と最大。`~/.local/share/dagq-hostmetrics/metrics.csv` は 2026-10-03 21:01 で止まっているので、queue の dir の `host/metrics-YYYYMMDD.csv`（supervisor が書く CSV。queue.db は開かない）も `--metrics` に渡す。
- 区間の境目は着地の commit の時刻（worktree の `git log --grep 'Dagq-Task: <ID>'`）。並列数は dagq.toml の `NEXTEST_TEST_THREADS` の履歴（4 は 884b3d56、8 は 6cdf238f、6 は 40f693f2 から。以降は変わっていない）を test 段の開始の時刻に当てる。
- test ごとの秒は区間の中の中央値で、本数と合計はその中央値の数と和（(c)〜(f)）。

選んだ log は [goal-119/logs.csv](it-reduction/goal-119/logs.csv)（相対 path・mtime・区間・並列数・本数・Summary の秒・test の時間の合計・dagq::it と対象の module の秒・load1 の平均と最大）、除いた log は [goal-119/excluded.csv](it-reduction/goal-119/excluded.csv)。

### 区間

| 区間 | log の本数 | mtime（JST） | 含む goal 119 の着地 | 並列数 |
| --- | ---: | --- | --- | ---: |
| 前 | 20 | 2026-10-04 14:57:04 〜 23:16:48 | なし（最後の 1 本は task 1709 の着地 6e53bc4d 自身の関門） | 6 |
| 4 本の後 | 20 | 2026-10-05 02:34:42 〜 09:59:51 | 6e53bc4d（1709）・e70840a9（1712）・04d8bb4e（1713）・824619bf（1711） | 6 |
| 後 | 1 | 2026-10-07 11:18:18 | 上の 4 本と cf96001d（1710） | 6 |

着地の時刻: 1709 が 2026-10-04 23:16:53、1712 が 10-05 00:06:48、1713 が 01:03:05、1711 が 01:45:54、1710 が 10-07 10:54:22。main の着地（`git log --first-parent main` を区間の最初と最後の log の mtime で区切って数えた）は、前の区間の間に 29 本、4 本の後の区間の間に 29 本。前の区間の最後の 1 本は 1709 の変更を含む木の関門なので、厳密には変更の前ではない（20 本の 1 本で、test ごとの中央値への影響は小さい）。1709 と 1711 の間（10-05 00:06〜01:45）の 4 本と、1711 と 1710 の間の残りの 50 本は logs.csv に `middle` として残す。

### (a)〜(d)・(g) 区間ごとの値

| 区間 | log | 並列数 | (a) Summary の中央値（範囲） | (b) test の時間の合計の中央値（範囲） | (c) dagq::it の本数・合計 | (d) lib の dagq の本数・合計 | (g) load1 の平均の中央値（範囲）・最大の中央値 |
| --- | ---: | ---: | --- | --- | --- | --- | --- |
| request 20 のとき（参考、直近 10 本） | 10 | 6 | 範囲だけ 400〜640 | 3,194 | 1,186・3,029（log ごとの合計） | 1,509・97 | — |
| 前 | 20 | 6 | 547.0（373.9〜641.0） | 3,223.1（2,180.3〜3,808.4） | 1,193・2,802.0 | 1,541・104.0 | 12.93（5.54〜20.60）・18.62 |
| 4 本の後 | 20 | 6 | 417.4（339.2〜710.7） | 2,446.0（1,977.8〜4,212.7） | 1,038・2,213.9 | 1,571・97.5 | 13.21（7.53〜41.44）・16.41 |
| 後 | 1 | 6 | 389.5 | 2,279.1 | 1,073・2,133.9 | 1,711・93.9 | 7.74・9.64 |
| 差（4 本の後 − 前） | | | −129.6（23.7%） | −777.1（24.1%） | −155 本・−588.1（21.0%） | +30 本・−6.5 | |
| 差（後 − 前、暫定） | | | −157.5（28.8%） | −944.0（29.3%） | −120 本・−668.1（23.8%） | +170 本・−10.1 | |

括弧の % は短縮率（1 − 後 ÷ 前）。(c)(d) は test ごとの中央値の数と和で、request 20 の 3,029 秒（log ごとの合計の中央値）と同じ統計量ではない。log ごとの dagq::it の合計の中央値は前 2,964.9 秒、4 本の後 2,280.1 秒、後 2,133.9 秒。test 段の本数（Summary の tests run の中央値）は前 2,874、4 本の後 2,749、後 2,981。flaky は前 0・4 本の後 1、最終の FAIL は前に 1 本（11752111 の 1 回目）。

### (e) 対象の module

test ごとの中央値の本数と合計（秒）。対象の module の全体で、前 189 本・429.3 秒、4 本の後 148 本・282.6 秒（−146.7 秒・34.2%）、後 131 本・231.8 秒（−197.5 秒・46.0%）。

| module | task | 前 本 | 前 秒 | 4 本の後 本 | 4 本の後 秒 | 差 | 後 本 | 後 秒 | 差 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `cli_stats` | 1709 | 12 | 1.0 | 3 | 0.7 | −0.3 | 3 | 0.7 | −0.3 |
| `installed_plugin` | 1709 | 7 | 1.7 | 5 | 1.3 | −0.4 | 5 | 1.4 | −0.3 |
| `runtime_claim` | 1709 | 26 | 40.5 | 23 | 33.4 | −7.1 | 22 | 28.8 | −11.7 |
| `runtime_heartbeat` | 1709 | 4 | 2.1 | 4 | 2.2 | +0.1 | 4 | 2.2 | +0.1 |
| `cli_authorization` | 1709 | 11 | 8.7 | 10 | 4.0 | −4.7 | 11 | 3.9 | −4.8 |
| `cli_actor` | 1709 | 5 | 9.9 | 4 | 5.2 | −4.7 | 4 | 4.6 | −5.3 |
| `plan_review` | 1710 | 30 | 72.4 | 30 | 65.4 | −7.0 | 16 | 34.5 | −37.9 |
| `plan_review_concern` | 1710 | 4 | 7.2 | 4 | 6.2 | −1.0 | 1 | 1.0 | −6.2 |
| `plan_review_codex` | 1710 | 4 | 5.2 | 4 | 4.7 | −0.5 | 3 | 3.3 | −1.9 |
| `plan_review_reasons` | 1710 | 1 | 2.4 | 1 | 1.9 | −0.5 | 0 | 0 | −2.4 |
| `plan_review_prompt_bytes` | 1710 | 2 | 2.0 | 2 | 1.7 | −0.3 | 0 | 0 | −2.0 |
| `planner_headless_turns` | 1711 | 9 | 99.3 | 5 | 38.2 | −61.1 | 5 | 37.2 | −62.1 |
| `request_planner` | 1711 | 7 | 14.0 | 7 | 14.1 | +0.1 | 8 | 12.7 | −1.3 |
| `planner_headless_stats` | 1711 | 1 | 9.2 | 1 | 8.6 | −0.6 | 1 | 9.2 | +0.0 |
| `review_subagents` | 1712 | 17 | 47.0 | 11 | 23.1 | −23.9 | 12 | 23.1 | −23.9 |
| `goal_review` | 1712 | 13 | 24.4 | 11 | 16.5 | −7.9 | 12 | 14.5 | −9.9 |
| `goal_review_codex` | 1712 | 7 | 8.7 | 4 | 6.7 | −2.0 | 4 | 6.3 | −2.4 |
| `runtime_codex` | 1713 | 16 | 40.2 | 11 | 27.8 | −12.4 | 11 | 25.1 | −15.1 |
| `runtime_provider_switch` | 1713 | 13 | 33.5 | 8 | 20.9 | −12.6 | 9 | 23.1 | −10.4 |

4 本の後の plan_review の群は 1710 の着地の前なので、本数が変わらず秒だけが動いている（load と同じ時期の別の着地の分）。`runtime_claim`・`cli_authorization`・`cli_actor` の本数の変化には、同じ時期の別の着地（task 1437 の対話の worker の廃止など）で消えた・足された test を含む。

### (f) 前と後のあいだに消えた・足された dagq::it の test

前の区間にあり後に無い名前（消えた）と、後にあり前に無い名前（足された）。秒は、消えたものは前の、足されたものは後の test ごとの中央値の和。名前を変えた test は消えたと足されたの両方に入る。

| 範囲 | 4 本の後: 消えた | 4 本の後: 足された | 4 本の後: 差 | 後: 消えた | 後: 足された | 後: 差 |
| --- | --- | --- | --- | --- | --- | --- |
| 対象の module | 45 本・124.4 | 4 本・5.0 | −41 本・−119.4 | 70 本・173.2 | 12 本・21.9 | −58 本・−151.3 |
| 対象の外の module | 173 本・549.2 | 59 本・152.6 | −114 本・−396.6 | 204 本・607.2 | 142 本・278.2 | −62 本・−329.0 |
| dagq::it の計 | 218 本・673.6 | 63 本・157.6 | −155 本・−516.0 | 274 本・780.4 | 154 本・300.2 | −120 本・−480.2 |
| 両方にある dagq::it（前 → 後の秒） | 975 本: 2,128.3 → 2,056.3（−72.0） | | | 919 本: 2,021.5 → 1,833.7（−187.8） | | |

対象の module ごと（本・秒）:

| module | 4 本の後: 消えた | 4 本の後: 足された | 後: 消えた | 後: 足された |
| --- | --- | --- | --- | --- |
| `planner_headless_turns` | 4・57.4 | — | 4・57.4 | — |
| `plan_review` | — | — | 17・34.7 | 3・9.5 |
| `review_subagents` | 6・20.3 | — | 6・20.3 | 1・1.6 |
| `runtime_codex` | 6・13.5 | 1・2.4 | 6・13.5 | 1・1.6 |
| `runtime_provider_switch` | 5・13.4 | — | 5・13.4 | 1・4.4 |
| `runtime_claim` | 5・7.0 | 2・1.8 | 6・10.0 | 2・1.8 |
| `plan_review_concern` | — | — | 3・6.2 | — |
| `cli_authorization` | 1・4.6 | — | 1・4.6 | 1・0.2 |
| `goal_review` | 3・3.3 | 1・0.8 | 3・3.3 | 2・2.2 |
| `cli_actor` | 1・2.7 | — | 1・2.7 | — |
| `plan_review_reasons` | — | — | 1・2.4 | — |
| `goal_review_codex` | 3・2.1 | — | 3・2.1 | — |
| `plan_review_prompt_bytes` | — | — | 2・2.0 | — |
| `plan_review_codex` | — | — | 1・0.4 | — |
| `cli_stats` | 9・0.2 | — | 9・0.2 | — |
| `installed_plugin` | 2・0.0 | — | 2・0.0 | — |
| `request_planner` | — | — | — | 1・0.7 |

対象の外で消えた秒の大きい module（4 本の後）は `runtime_session` 18 本・49.7、`runtime_handoff` 10・45.8、`runtime_resume_exit_retry` 7・39.1、`runtime_stall_recovery` 11・37.5、`runtime_exit_retry` 11・31.9、`runtime_resume_adopt` 6・31.6、`runtime_resume` 5・30.1、`runtime_screen_idle_input` 8・29.1。足された（増え直し）のは `runtime_recheck` 4・19.5、`runtime_resume` 3・18.6、`runtime_headless_stall` 5・17.5、`runtime_handoff` 4・17.3、`runtime_broker` 9・16.8、`runtime_review_conflict` 4・12.6。後の 1 本では `recovery_codex` 11・26.0、`draft_revisit` 3・18.2 なども足されている。全部の module の値はスクリプトの `diff` の行が出す。

移し替えの効果の見積もりは、対象の module の「消えた − 足された」（4 本の後 −119.4 秒、後 −151.3 秒）。dagq::it の合計の差は、それと対象の外の差と両方にある test の変化の和になる（4 本の後: −119.4 − 396.6 − 72.0 = −588.0、後: −151.3 − 329.0 − 187.8 = −668.1）。

### 各 task の交互 3 回の A/B

1709〜1713 の receipt の summary（着地の commit の本文。`git log --grep 'Dagq-Task: <ID>'`、run は Dagq-Run の trailer）の module ごとの表を写した。どの task も module ごとの値を残している。NEXTEST_TEST_THREADS=6、base と変更後を交互に各 3 回、秒は module の PASS・FLKY-FL・FLAKY の行の秒の和の中央値。

| task | module | 件数 base | 件数 変更後 | 秒 base | 秒 変更後 | 差 | 短縮率 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1709 | `cli_stats` | 12 | 3 | 1.247 | 0.983 | −0.264 | 21.2% |
| 1709 | `installed_plugin` | 7 | 5 | 1.306 | 1.666 | +0.360 | −27.6% |
| 1709 | `runtime_claim` | 25 | 25 | 38.940 | 54.825 | +15.885 | −40.8% |
| 1709 | `runtime_heartbeat` | 4 | 4 | 2.163 | 2.155 | −0.008 | 0.4% |
| 1709 | `cli_authorization` | 10 | 10 | 10.234 | 7.780 | −2.454 | 24.0% |
| 1709 | `cli_actor` | 4 | 4 | 8.247 | 8.195 | −0.052 | 0.6% |
| 1710 | `plan_review` | 29 | 16 | 62.926 | 38.716 | −24.210 | 38.5% |
| 1710 | `plan_review_concern` | 4 | 1 | 5.495 | 0.929 | −4.566 | 83.1% |
| 1710 | `plan_review_codex` | 4 | 3 | 4.591 | 3.077 | −1.514 | 33.0% |
| 1710 | `plan_review_reasons` | 1 | 0 | 1.535 | 0 | −1.535 | 100% |
| 1710 | `plan_review_prompt_bytes` | 2 | 0 | 1.509 | 0 | −1.509 | 100% |
| 1711 | `planner_headless_turns` | 9 | 5 | 88.472 | 38.014 | −50.458 | 57.0% |
| 1711 | `request_planner` | 7 | 7 | 11.066 | 14.457 | +3.391 | −30.6% |
| 1711 | `planner_headless_stats` | 1 | 1 | 9.650 | 8.321 | −1.329 | 13.8% |
| 1712 | `review_subagents` | 17 | 11 | 51.004 | 25.561 | −25.443 | 49.9% |
| 1712 | `goal_review` | 13 | 11 | 36.705 | 25.557 | −11.148 | 30.4% |
| 1712 | `goal_review_codex` | 7 | 4 | 9.486 | 7.134 | −2.352 | 24.8% |
| 1713 | `runtime_codex` | 16 | 10 | 40.999 | 33.292 | −7.707 | 18.8% |
| 1713 | `runtime_provider_switch` | 12 | 8 | 24.313 | 17.573 | −6.740 | 27.7% |

module ごとの差の和は 1709 +13.467、1710 −33.334、1711 −48.396、1712 −38.943、1713 −14.447、計 −121.653 秒（中央値の和で、同じ回の値の和ではない）。1709 の差はどの module も 3 回の幅に収まり、receipt は揺れの範囲で差は見えないとしている。

実行群（task ごとの module を合わせた 1 回の nextest）の Summary の壁時計と test の時間の合計は module に割らず、task ごとの行で並べる。

| task | 実行群 | Summary base | Summary 変更後 | 差 | 短縮率 | 合計 base | 合計 変更後 | 差 | 短縮率 | load1（receipt の記録） |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 1709 | 6 module | 12.548 | 15.082 | +2.534 | −20.2% | 62.032 | 75.604 | +13.572 | −21.9% | 17〜23 |
| 1710 | 5 module | 13.475 | 7.544 | −5.931 | 44.0% | 76.133 | 42.722 | −33.411 | 43.9% | 4.7〜6.5 |
| 1711 | 3 module | 21.812 | 14.195 | −7.617 | 34.9% | 109.940 | 61.020 | −48.920 | 44.5% | 3.5〜6.5 |
| 1712 | 3 module | 18.586 | 13.362 | −5.224 | 28.1% | 97.195 | 60.349 | −36.846 | 37.9% | 7〜11 |
| 1713 | 2 module | 11.453 | 9.322 | −2.131 | 18.6% | 65.312 | 50.865 | −14.447 | 22.1% | 6〜12 |

### Summary との照合

関門の log（Summary を持ち、前の区間の 7 日前より後のもの）511 本のうち、471 本が照合を通り、40 本を除いた（excluded.csv）。

| 理由 | 本数 |
| --- | ---: |
| canceled（`N/M tests run`、途中で止まった関門） | 16 |
| no metrics in the test stage（test 段の間に load の標本が無い。host の CSV の欠け） | 23 |
| 0 Summary lines（`Summary [` を含むが nextest の Summary の行ではない log） | 1 |

採った件数＋最終の FAIL の件数が `N tests run` と合わない log と、PASS の件数が passed − FLAKY と合わない log は 0 本。load の無い log のうち前の区間の期間にある 1 本（10-04 19:30）と 4 本の後の区間の期間にある 4 本（10-05 03:43・04:54・05:07・06:23）は区間から外れ、区間の 20 本はその前・後の log で埋めた。

### 測り直し

後の区間は 1 本で暫定。最後の着地（cf96001d）の後の 20 本がそろったら、同じコマンドで測り直して、この節の後の行と冒頭の結論を置き換える（follow_up、category measurement）。スクリプトは後の区間を最後の着地の後の最初の 20 本に取るので、`--until` を外すか 20 本目の後の時刻にする。この節の値は `--until 2026-10-07T11:19:00+09:00` で読んだもの（その後の 12:02 に 2 本目の log が出ている）。
