---
id: plan-it-reduction
type: plan
title: tests/it の全 1,192 本の分類（境界・判断・代表あり・goal 92 で消える）と、it でないと担保できない test の見積もり
status: active
created: 2026-10-04
updated: 2026-10-04
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
