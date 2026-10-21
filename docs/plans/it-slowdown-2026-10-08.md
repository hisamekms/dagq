---
id: plan-it-slowdown-2026-10-08
type: plan
title: 2026-10-08 から同じ IT が 1 本あたり遅くなった原因を、着地の検証のログと再現で調べた記録
status: completed
created: 2026-10-22
owners:
  - hisamekms
tags:
  - testing
  - performance
  - measurement
related:
  - plan-landing-it-alternatives
  - adr-t1925-1
  - adr-t1433-4
---

# 2026-10-08 からの IT の遅さの原因

[着地の IT の全部流す条件の代替案](landing-it-alternatives.md)の「全部流した run の段」は、前後の窓の両方で流れた同じ IT 1,137 本の中央値の和が 2,319 → 3,253 秒（×1.40）に伸び、IT 1 本あたりの秒が 10-08 から上がったことを示したが、何が遅くしたかは分けていなかった。
ここでは着地の検証のログから伸びの起きた時刻を run 単位まで絞り、候補を記録と再現で確かめた。

## 要点

- **伸びは 2026-10-08T11:13Z と 11:42Z の間の 1 段で起き、その間に supervisor が in_cmux mode から launchd mode に替わった。**
  in_cmux の supervisor は 10:45:10Z に drain に入り、launchd mode の supervisor が 11:14:34Z に起動した（`supervisor_started` の `mode`）。
  in_cmux の supervisor の下の最後の検証（11:13Z に終わった run）までは前の窓に対する比が 1 前後で、launchd の下の最初の検証（11:42Z）から 1.9 前後になり、そのまま続いた。
- **launchd の LaunchAgent の plist に `ProcessType` が無く、launchd はその job と子の process に軽い資源の制限（CPU と I/O の抑制）を掛ける**（`man launchd.plist` の `ProcessType`: 「If left unspecified, the system will apply light resource limits to the job, throttling its CPU usage and I/O bandwidth.」）。
  今の host で、launchd の supervisor（pid 679）と、それが起動した worker の session・shell は優先度（`ps -o pri`）20 で、cmux の中の shell と Claude は 31 だった。検証の `sh scripts/landing-it.sh` と nextest と各 test は supervisor の子なので 20 で流れる。
- **記録での比較**: 同じ `landing-it.sh` の全部流した回を supervisor の mode で分けると、前の窓の llvm-cov の中央値に対する 1 本ごとの比の中央値は in_cmux の 60 回で 1.02、launchd の 221 回で 1.87。load の帯をそろえても差は残る（load の平均が 10 未満の回で 0.93 → 1.52、10〜13 で 1.02 → 1.74）。
- **再現**: 変化の前後の commit（506d4050・0f0d3c76・53dffb94）の IT を同じ host で交互に各 4 周流しても、1 本あたりの秒の中央値は 6.42・6.46・6.41 秒で差が無かった。test の binary の差分（runtime や test の helper）は原因でない。
  同じ binary を優先度 20（この run の経路の既定）と `taskpolicy -b`（優先度 4）で交互に各 3 周流すと、中央値は 4.31 → 15.48 秒（×3.6）になり、IT の秒は process の QoS に強く依る。31 で流す経路はこの run から作れず、31 と 20 の差の大きさは測っていない（下の「再現」）。
- **結論**: 1 本あたりの伸びの原因は、supervisor が launchd mode で常駐するようになり、着地の検証が launchd の既定の制限（優先度 20 の帯）の下で流れるようになったことと見る。時刻が回の単位で一致し、load をそろえた mode の比較でも 1.6〜1.9 倍の差が残ることが根拠で、31 と 20 を同じ時に流した直接の比較は無い。
  寄与は伸び（×1.4〜1.9）の大部分で、残りは load（launchd の下でも load の帯で 1.52〜2.39 と動く）。着地の検証の切り替え（landing-it.sh）、10-08〜10-09 の runtime の test の binary の変更、nextest の並列度・profile、sccache・container は、記録か再現で当たらない。

## 窓と入力

- **ログ**: 2026-10-01T00:00Z 以上 2026-10-20T00:00Z 未満（UTC）の `verification_command` のうち、`command` が `cargo llvm-cov nextest` で始まり `exit_code` 0 のもの（前の窓、着地の検証の切り替えの印 `2026-10-07T10:27:40.972Z` より前）と、`sh scripts/landing-it.sh` のもの（後の窓、`exit_code` は問わず PASS の行だけを数える）で、どちらも PASS の IT が 500 本以上の回（landing-it では IT を全部流した回）。
  使ったログは前の窓 275 件、後の窓 281 件（日ごとの件数と最初と最後の時刻は下の表）。10-10 の着地の検証は 0 件だった。ログは `log_path`（host の run の directory）にすべて残っていた。
- **共通の IT**: 前の窓と後の窓の両方で PASS した IT（binary id と名前の組）1,137 本。後の窓で test の名前の変更と削除が進み、日ごとの本数は 10-15 から 738 本に減る。本数の変化で値がずれないように、全部の日で流れた 474 本の値も並べる。
- **並列度**: どの回も `dagq.toml` の `[run.env]` の `NEXTEST_TEST_THREADS = "6"`・`RUST_TEST_THREADS = "6"`（2026-09-28T20:26Z の 40f693f2 から窓の間に変わっていない）。`.config/nextest.toml` は窓の間に `profile.ci.junit` を足しただけ（5e4125b3）で、検証が使う `profile.default` は変わっていない。ログの nextest の profile はどの回も default。
- **mode の区切り**: `supervisor_started` の `mode` が launchd になった最初の時刻 `2026-10-08T11:14:34.532Z`。回の時刻は `verification_command` の `created_at`（終わった時刻）。
- 読んだ時刻は 2026-10-21T18:00Z〜19:30Z（UTC）。状態を変えないコマンド（`events`）だけを worker のクライアントモードで読んだ。
- **周回しない理由**: 表の値は残ったログを 1 回読んで計算するだけで決まり、揺れの源が無いので周回数は当たらない（1 回）。周回したのは下の「再現」だけ。

## 値の式

- **1 本あたりの秒**（日ごと）= その日の回で PASS した共通の IT について、IT ごとのその日の中央値の和 ÷ その本数。module の表は同じ式を module の IT に当てた値で、括弧は本数。
- **伸びた上位 10 module**: 共通の IT ごとの（後の窓の中央値 − 前の窓の中央値）を module で足し、大きい順に 10。[stage-summary.json](landing-it-alternatives/stage-summary.json) の `modules_grown_most` と同じ式で、上位 4 は一致する（5 位以下は拾う回の違いで入れ替わる。stage-summary.json の 5 位の `runtime_headless` はここでは 6 位）。
- **回の比**: 後の窓の各回で、共通の IT の（その回の秒 ÷ 前の窓の中央値）の中央値を取り、その回の比とする（`by-day.json` の `runs_after`）。mode ごと、mode と load の平均の帯ごとに回の比の中央値と p90 を出す（`mode_split`）。
- 中央値は件数が偶数なら中の 2 つの平均、p90 は最近順位法（昇順の ceil(0.9×n) 番目）。

## 日ごとの値

`it-slowdown-2026-10-08/by-day.json` が下の 2 つの表と mode ごとの比を持つ。

| 日（UTC） | 窓 | log の件数 | 最初〜最後の終わりの時刻 | 共通の IT の本数 | 1 本あたりの秒 | 全部の日で流れた 474 本の 1 本あたりの秒 | load の平均の中央値 | nextest の秒の中央値 |
|---|---|---|---|---|---|---|---|---|
| 2026-10-01 | 前（llvm-cov） | 35 | 14:15〜23:57 | 753 | 1.74 | 1.14 | 15.1 | 400.1 |
| 2026-10-02 | 前（llvm-cov） | 58 | 00:17〜23:54 | 857 | 1.69 | 1.06 | 13.3 | 377.0 |
| 2026-10-03 | 前（llvm-cov） | 50 | 00:11〜23:52 | 922 | 2.04 | 1.24 | 16.3 | 482.2 |
| 2026-10-04 | 前（llvm-cov） | 59 | 00:06〜23:50 | 1016 | 2.08 | 1.20 | 11.9 | 467.3 |
| 2026-10-05 | 前（llvm-cov） | 30 | 00:09〜23:51 | 1072 | 2.33 | 1.33 | 12.6 | 478.3 |
| 2026-10-06 | 前（llvm-cov） | 18 | 00:20〜23:48 | 1112 | 2.12 | 1.23 | 11.2 | 455.0 |
| 2026-10-07 | 前（llvm-cov） | 25 | 00:15〜09:54 | 1137 | 1.95 | 1.12 | 8.6 | 405.4 |
| 2026-10-07 | 後（landing-it） | 29 | 11:01〜23:59 | 1137 | 2.12 | 1.22 | 11.4 | 448.8 |
| 2026-10-08 | 後（landing-it） | 56 | 00:24〜23:51 | 1128 | 2.58 | 1.52 | 11.7 | 573.6 |
| 2026-10-09 | 後（landing-it） | 22 | 00:30〜11:09 | 1031 | 3.08 | 1.83 | 12.2 | 660.7 |
| 2026-10-11 | 後（landing-it） | 17 | 06:59〜23:56 | 989 | 3.25 | 1.85 | 12.7 | 633.6 |
| 2026-10-12 | 後（landing-it） | 34 | 00:16〜23:38 | 983 | 3.42 | 1.97 | 13.3 | 703.2 |
| 2026-10-13 | 後（landing-it） | 3 | 15:19〜17:24 | 973 | 4.36 | 2.53 | 21.1 | 952.1 |
| 2026-10-14 | 後（landing-it） | 8 | 00:30〜10:53 | 973 | 4.76 | 2.85 | 23.6 | 662.6 |
| 2026-10-15 | 後（landing-it） | 5 | 04:55〜16:55 | 738 | 2.62 | 2.44 | 17.0 | 491.8 |
| 2026-10-16 | 後（landing-it） | 26 | 01:00〜22:13 | 738 | 2.86 | 2.66 | 20.1 | 534.3 |
| 2026-10-17 | 後（landing-it） | 23 | 00:01〜23:33 | 738 | 2.86 | 2.66 | 18.8 | 563.5 |
| 2026-10-18 | 後（landing-it） | 28 | 00:05〜23:55 | 738 | 2.40 | 2.22 | 13.3 | 485.3 |
| 2026-10-19 | 後（landing-it） | 30 | 01:14〜23:36 | 737 | 2.13 | 1.97 | 10.4 | 443.0 |

- 10-08 の行は in_cmux の下の回（11:13Z まで）と launchd の下の回を合わせた値で、段の途中にある。
- 10-15 からの「1 本あたりの秒」の下がりは、遅い module の IT（`runtime_resume` など）の名前が変わって共通から外れたことによる。474 本の値は 10-19 でも 1.97 で、前の窓（1.06〜1.33）に戻っていない。
- 10-13・10-14 は load が 21〜24 と高く、その分さらに遅い。

伸びた上位 10 module の 1 本あたりの秒（括弧は共通の IT の本数、— はその日に共通の IT が流れていない）:

| 日（UTC） | 窓 | `runtime_resume` | `runtime_integrate` | `runtime_recheck` | `runtime_handoff` | `runtime_claim_defer` | `runtime_headless` | `runtime_triage` | `queue_service_reads` | `runtime_claim` | `runtime_review` |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 2026-10-01 | 前 | 5.48（15） | 3.59（21） | 4.88（3） | 3.32（8） | 6.86（2） | 3.20（16） | 1.97（18） | 5.43（3） | 1.48（19） | 3.72（10） |
| 2026-10-02 | 前 | 4.81（16） | 2.81（23） | 3.85（10） | 2.68（14） | 6.14（2） | 2.83（16） | 1.81（19） | 5.66（3） | 1.37（19） | 3.43（11） |
| 2026-10-03 | 前 | 4.87（16） | 3.14（23） | 4.00（10） | 3.17（14） | 3.63（6） | 2.90（17） | 1.88（19） | 5.90（3） | 1.54（19） | 2.88（11） |
| 2026-10-04 | 前 | 5.25（20） | 3.11（23） | 4.25（14） | 3.51（18） | 3.62（8） | 2.88（20） | 1.95（20） | 5.77（3） | 1.39（22） | 2.86（11） |
| 2026-10-05 | 前 | 5.89（20） | 3.52（23） | 4.66（14） | 3.89（18） | 4.20（8） | 3.40（20） | 2.28（21） | 5.87（3） | 1.58（22） | 3.27（11） |
| 2026-10-06 | 前 | 5.61（20） | 3.06（23） | 4.42（14） | 3.63（18） | 3.78（8） | 3.04（20） | 2.34（21） | 5.75（3） | 1.44（22） | 2.98（11） |
| 2026-10-07 | 前 | 5.16（20） | 3.07（23） | 4.28（16） | 3.21（18） | 3.57（9） | 2.69（20） | 2.10（21） | 3.92（5） | 1.37（22） | 2.78（14） |
| 2026-10-07 | 後 | 5.70（20） | 3.35（23） | 4.58（16） | 3.58（18） | 4.03（9） | 3.01（20） | 2.18（21） | 3.78（5） | 1.55（22） | 3.07（14） |
| 2026-10-08 | 後 | 6.84（20） | 4.04（23） | 6.00（16） | 4.71（18） | 5.02（9） | 3.88（20） | 2.82（21） | 4.23（5） | 1.88（22） | 3.86（14） |
| 2026-10-09 | 後 | 7.70（20） | 4.88（23） | 7.10（16） | 5.83（18） | 6.65（9） | 4.85（20） | 3.36（21） | 9.93（5） | 2.31（22） | 4.42（14） |
| 2026-10-11 | 後 | 8.91（20） | 5.44（23） | 8.85（10） | 6.17（18） | 7.32（9） | 5.05（20） | 4.00（21） | 8.39（5） | 2.39（22） | 4.96（14） |
| 2026-10-12 | 後 | 9.51（20） | 5.69（23） | 9.86（10） | 6.34（18） | 7.71（9） | 5.22（20） | 4.13（21） | 9.56（5） | 2.56（22） | 5.16（12） |
| 2026-10-13 | 後 | 11.79（20） | 7.38（23） | 12.50（10） | 7.22（15） | 10.81（9） | 6.87（20） | 5.37（21） | 9.88（5） | 3.71（21） | 6.22（12） |
| 2026-10-14 | 後 | 13.28（20） | 8.01（23） | 14.00（10） | 7.46（15） | 11.88（9） | 7.06（20） | 6.11（21） | 10.96（5） | 3.94（21） | 7.07（12） |
| 2026-10-15 | 後 | — | 5.32（5） | 7.06（2） | 3.67（8） | 10.76（9） | 4.64（4） | 3.52（13） | 9.89（5） | 3.64（21） | 4.66（3） |
| 2026-10-16 | 後 | — | 5.90（5） | 7.93（2） | 3.91（8） | 11.28（9） | 5.14（4） | 3.73（13） | 10.50（5） | 3.99（21） | 5.06（3） |
| 2026-10-17 | 後 | — | 6.36（5） | 8.46（2） | 3.96（8） | 11.31（9） | 5.22（4） | 3.63（13） | 10.84（5） | 4.10（21） | 5.16（3） |
| 2026-10-18 | 後 | — | 5.18（5） | 6.66（2） | 3.31（8） | 9.19（9） | 4.23（4） | 3.09（13） | 10.39（5） | 3.13（21） | 4.22（3） |
| 2026-10-19 | 後 | — | 4.41（5） | 5.93（2） | 3.09（8） | 8.25（9） | 3.67（4） | 2.77（13） | 9.50（5） | 2.98（21） | 3.64（3） |

## 伸びの起きた時刻

後の窓の回の比（`by-day.json` の `runs_after`）を時刻順に並べると、10-07T11:01Z（切り替えの後の最初の回）から 10-08T11:13Z までの in_cmux の supervisor の下の 60 回は 0.81〜1.59（中央値 1.02）で、10-08T11:42Z の回から上がり、10-08〜10-09 の launchd の下の回は 1.22〜2.38 だった。10-19 までの launchd の下の 221 回では 1.01〜5.91 で、1 前後に戻った回は少ない（p90 は下の表）。
回の rebase 先の main（その回の `integration_started` の `main`、`dagq events --kind integration_started` で別に読んだ）は、11:13Z の回が 506d4050、11:42Z の回が 0f0d3c76 だった。

| 終わった時刻（UTC） | rebase 先の main | 比 | load の平均 | supervisor の mode |
|---|---|---|---|---|
| 10-08T10:36:54Z | ad333c85 | 1.02 | 12.6 | in_cmux |
| 10-08T10:51:39Z | 8e24119c | 0.96 | 8.2 | in_cmux |
| 10-08T11:13:00Z | 506d4050 | 0.84 | 6.1 | in_cmux（drain 中） |
| 10-08T11:42:54Z | 0f0d3c76 | 1.94 | 18.0 | launchd |
| 10-08T11:57:27Z | 0f0d3c76 | 2.03 | 17.7 | launchd |
| 10-08T12:17:42Z | 53dffb94 | 1.65 | 14.0 | launchd |

段の直後の 2 回は load も高い（18）ので、load をそろえて mode を比べた（`by-day.json` の `mode_split`）:

| load の平均の帯 | in_cmux 回 | in_cmux 比の中央値（p90） | launchd 回 | launchd 比の中央値（p90） |
|---|---|---|---|---|
| 10 未満 | 16 | 0.93（0.98） | 34 | 1.52（1.68） |
| 10〜13 | 27 | 1.02（1.16） | 67 | 1.74（1.95） |
| 13〜16 | 3 | 1.19（1.20） | 40 | 1.92（2.16） |
| 16 以上 | 14 | 1.22（1.51） | 80 | 2.39（2.85） |
| 全部 | 60 | 1.02（1.25） | 221 | 1.87（2.52） |

- 同じ load の帯で launchd の回は in_cmux の回の 1.6〜2.0 倍。mode の差は load では説明できない。
- launchd の下でも load の帯で 1.52〜2.39 と動き、load は 2 番目の要因として残る。遅くなった test が長く走ること自体が load を上げる分もありうる。
- in_cmux の回は 10-07T11:01Z〜10-08T11:13Z、launchd の回は 10-08T11:42Z〜10-19 で、時期も違う。

## 候補ごとの当否

- **(a) 着地の検証の切り替え（ec310498、10-07T10:27Z）: 当たらない。**
  切り替えの後、in_cmux の supervisor の下の landing-it の 60 回は比の中央値 1.02 で、llvm-cov の回と同じ速さだった。伸びは切り替えから 25 時間後に 1 段で起きた。
  並列度（`NEXTEST_TEST_THREADS` 6）・nextest の profile（default）・build の profile（`test`、unoptimized + debuginfo）・`[run.env]` はどちらの流し方でも同じ。違いは llvm-cov の計測の有無と、landing-it が workspace の lib・bin と `kind(test) - binary(e2e)` の filter で流すことだけで、計測を外したのに遅くなる向きの説明にならない。
- **(b) 10-08〜10-09 の runtime の着地: 当たらない。**
  4cdd183b（resource broker の撤去、10-08T07:01Z）と c35ee27c（10-08T03:17Z）は段の前に入り、その後の回も比は 1 前後だった。cde598ae（in-cmux mode の廃止、10-08T10:09Z）も段の前に main に入り、その後の 3 回（ad333c85・8e24119c・506d4050 に rebase）は 0.84〜1.02 だった。26184a7b（10-08T22:17Z）と cccef3ef（10-08T23:57Z）は段の後に入り、前後で比の幅は変わっていない（10-08〜10-09 の launchd の下の回は 1.22〜2.38）。
  段の直前と直後の main（506d4050・0f0d3c76）と次の着地（53dffb94）の IT を同じ host・同じ並列度で交互に流しても差は無かった（下の「再現」）。ただし再現で替えたのは test の binary だけで、検証を起動する supervisor の binary は替えていない。段のときは supervisor の binary も 8e24119c から 0f0d3c76 に替わり（11:21Z の `update_installed`）、cde598ae を含む。supervisor が検証の process を起動する仕方の変化は、ここでは mode（下の (d)）と分けていない。10-07〜10-09 に tests/common を変えた main の commit は、段の前の 860ac29e・c35ee27c・cde598ae（どれもその後の回の比は 1 前後）と、段の後の 10-09 の 2b19d29e・e4813469・5cfb3428（前後で比の幅は変わらない）で、段と重なるものは無い。
- **(c) host の変化: launchd mode への移行（下の (d)）が当たり、sccache・container は当たらない。load は主因ではないが 2 番目の要因として効く。**
  sccache は build の段だけに効き、テストの秒を変えない。後の窓の検証のログは host の path（`/Users/…`）で流れ、container の verification ではない。load をそろえても mode の差は残り（「伸びの起きた時刻」の帯ごとの表）、load の低い 10-19（10.4）でも 474 本の値は 1.97 だった。一方 launchd の下でも load の帯で比は 1.52〜2.39 と動き、10-13・10-14 の高い値は load（21〜24）が上乗せしている。
- **(d) 見つけた候補: supervisor が launchd mode で常駐するようになったこと（ADR-t1433-4、cde598ae）: 当たる。**
  in_cmux の supervisor は 10-08T10:45:10Z に `stop_requested` で drain に入り、launchd mode の supervisor が 11:14:34Z に起動し（`supervisor_started` の `mode`）、それ以降の `update_installed` の `supervisors` はどれも launchd だった。伸びの段はこの切り替わりと回の単位で一致する。
  LaunchAgent の plist（`~/Library/LaunchAgents/com.dagq.77067154921b9014.plist`）には `ProcessType` も `Nice` も無く、launchd の既定では CPU と I/O が抑えられる。今の host では launchd の supervisor・その子の worker の session と shell の優先度が 20（macOS の QoS の utility の帯）、cmux の中の shell・Claude が 31（default の帯）で、検証の nextest と test の process は supervisor の子として 20 で流れる。10-07〜10-08 の in_cmux の supervisor の子が 31 だったことは当時は記録が無く、今の cmux の中の process から推した値。Apple Silicon では utility の帯の process は効率コアに寄せて置かれやすく、I/O も抑えられるので、CPU の優先度だけでなくコアの選ばれ方と I/O の抑制が遅さに効きうる（どれが効いたかは分けていない）。
  `src` には優先度や QoS を設定する箇所（`ProcessType`・`taskpolicy`・`setpriority`）が無い。

## 再現

同じ worktree で commit を detached に checkout し、`cargo nextest archive --locked --test it` で commit ごとの IT の binary を別の target dir に作り（`CARGO_BUILD_JOBS=4`・sccache）、branch に戻してから `cargo nextest run --archive-file … --workspace-remap <worktree>` で流した。
流した IT は伸びの大きい `runtime_resume`（23 本）・`runtime_recheck`（16 本）・`runtime_handoff`（20 本）の 59 本で、`NEXTEST_TEST_THREADS=6`・`RUST_TEST_THREADS=6`。どれも全部 PASS した。
時刻は commit の前後が 2026-10-21T18:26Z〜18:51Z、QoS が 19:09Z〜19:20Z（UTC）、各周の開始時の load の 1 分平均は commit の前後が 5.8〜9.9、QoS が 2.5〜4.8。周回ごとの壁時計と load は `it-slowdown-2026-10-08/rounds.txt`、値は `repro-commits.json`・`repro-qos.json`。

commit の前後（A・B・C の順に交互に 4 周、優先度 20）:

| commit | 周 | nextest の壁時計（秒） | 1 本あたりの秒 中央値 | 同 p90 | `runtime_resume` 中央値 / p90 | `runtime_recheck` 中央値 / p90 | `runtime_handoff` 中央値 / p90 |
|---|---|---|---|---|---|---|---|
| A 506d4050（段の前の main） | 4 | 65.0・75.3・63.9・62.1 | 6.42 | 9.84 | 6.96 / 9.92 | 6.60 / 9.99 | 5.00 / 8.27 |
| B 0f0d3c76（段の後の main） | 4 | 66.1・104.6・62.2・63.5 | 6.46 | 9.63 | 6.67 / 10.17 | 6.46 / 9.16 | 5.84 / 8.40 |
| C 53dffb94（次の着地） | 4 | 65.8・73.9・62.8・62.7 | 6.41 | 9.65 | 6.63 / 10.01 | 6.15 / 9.35 | 5.92 / 9.14 |

QoS（C を、この run の経路の既定の優先度 20 と `taskpolicy -b` の優先度 4 で交互に 3 周）:

| 優先度 | 周 | nextest の壁時計（秒） | 1 本あたりの秒 中央値 | 同 p90 | `runtime_resume` 中央値 / p90 | `runtime_recheck` 中央値 / p90 | `runtime_handoff` 中央値 / p90 |
|---|---|---|---|---|---|---|---|
| 20（launchd の既定、今の検証と同じ） | 3 | 42.6・44.0・47.2 | 4.31 | 6.50 | 4.70 / 7.20 | 4.14 / 6.51 | 3.77 / 5.56 |
| 4（background） | 3 | 156.7・158.0・156.7 | 15.48 | 21.68 | 16.57 / 22.85 | 15.53 / 23.22 | 11.20 / 20.21 |

- commit の 3 つは 1 本あたりの中央値が 6.41〜6.46 秒で並び、差は周ごとの揺れ（壁時計 62〜105 秒）より小さい。runtime・test の変更は伸びを作っていない。
- B の 2 周目（104.6 秒）は 1 本が SLOW になった回で、その周の load は他と同じだった。
- 優先度を 20 から 4 に下げると同じ binary が 3.6 倍遅い。同じ C を同じ優先度 20 で流した値は、commit の前後の周（load 6〜10）で 6.41 秒、QoS の周（load 2.5〜4.8）で 4.31 秒で、load の違いでも 1.5 倍動く。IT は sqlite・git・子の process の起動と待ちが中心で、CPU の割り当てと I/O の抑制に敏感だと分かる。
- **流せなかったもの**: 優先度 31（in_cmux の supervisor の下と同じ帯）で流す経路はこの run から作れなかった。この run の session は launchd の supervisor の子で、`taskpolicy` は優先度を上げられず（`taskpolicy -t 0 -l 0` でも 20）、cmux の CLI は cmux の外の process を拒み（「cmux 内で起動されたプロセスのみ接続できます」）、別の LaunchAgent を載せることは host を変えるので worker がしない。
  31 と 20 の差は、代わりに記録の mode ごとの比（in_cmux 1.02・launchd 1.87、同じ landing-it.sh の全部流した回、load の帯をそろえて 1.6〜2.0 倍）で見た。20 と 4 の比較は QoS に敏感なことを示すだけで、31 と 20 の差の大きさは示さない。

## 結論

- **原因（最有力）**: 2026-10-08T11:14Z に supervisor が in_cmux mode から launchd mode に替わり、その子として流れる着地の検証の nextest と IT が、`ProcessType` の無い LaunchAgent への launchd の既定の制限（優先度 20 の帯、CPU と I/O の抑制、効率コアへの寄せ）の下で流れるようになったこと。
  根拠は、段の時刻が mode の切り替わりと回の単位で一致すること、load の帯をそろえても launchd の回が in_cmux の回の 1.6〜2.0 倍なこと、test の binary を替えても差が無いこと、IT の秒が QoS に強く依ること。31 と 20 を同じ時に流した直接の比較は無く、同じ時に替わった supervisor の binary（0f0d3c76）の検証の起動の仕方の変化とは分けていない。
- **寄与の見積もり**: load をそろえた mode の差（1.6〜2.0 倍）が、同じ IT の中央値の和の ×1.40 と 1 本ごとの比の中央値の 1.47（landing-it-alternatives の値）の大部分にあたる。残りは load で、launchd の下でも load の帯で 1.52〜2.39 倍と動く（10-13・10-14 の上乗せ）。着地の検証の切り替えと test の binary の変更の寄与は記録と再現の誤差の内（0 に近い）。
- worker・review・resume の session と、そこで流す手元の検査も同じ supervisor の子なので、同じ制限を受けていると見込む（測っていない）。
- **確かめきれていないこと**: 優先度 31 の下で今の IT を流した値（この run の経路から作れない）。次に測るものは、人の terminal（cmux の中、優先度 31）と launchd の子（20）で同じ archive を同じ時に交互に流す比較か、plist に `ProcessType` を足した後の着地の検証のログでの同じ比。

## 限界

- 前の窓の値は llvm-cov の計測つきの回で、後の窓は計測なしの回。in_cmux の下の landing-it の回（60 回）が 1.02 なので、計測の重さは小さいか他と打ち消している。
- 再現は 3 module の 59 本だけで、test が repository から読むファイルは今の branch の worktree のもの（`--workspace-remap`）だった。3 つの commit のどれも全部 PASS したので、読むファイルの違いは時間に効いていないと見る。
- 回の比は前の窓の IT ごとの中央値を分母にするので、前の窓に無い IT は入らない。

## 実行のコマンドとファイル

repository の root で（`EVENTS.json` は repository の外に置く材料）:

```sh
python3 - <<'PY' > EVENTS.json   # 12 時間ずつ verification_command を読む
import datetime as D, json, subprocess
s, t, out = D.datetime(2026, 10, 1), D.datetime(2026, 10, 20), []
while s < t:
    u = min(s + D.timedelta(hours=12), t)
    out += json.loads(subprocess.run(['dagq', 'events', '--full', '--kind', 'verification_command',
        '--since', s.strftime('%Y-%m-%dT%H:%M:%S.000Z'), '--until', u.strftime('%Y-%m-%dT%H:%M:%S.000Z'),
        '--limit', '200000'], capture_output=True, text=True, check=True).stdout)['events']
    s = u
json.dump(out, open('/dev/stdout', 'w'))
PY
python3 docs/plans/it-slowdown-2026-10-08/by_day.py EVENTS.json 2026-10-07T10:27:40.972Z 2026-10-08T11:14:34.532Z > docs/plans/it-slowdown-2026-10-08/by-day.json
python3 docs/plans/it-slowdown-2026-10-08/rounds.py LOGDIR r A B C > docs/plans/it-slowdown-2026-10-08/repro-commits.json
python3 docs/plans/it-slowdown-2026-10-08/rounds.py LOGDIR q u20 bg4 > docs/plans/it-slowdown-2026-10-08/repro-qos.json
```

- `by_day.py`: ログから日ごとの 1 本あたりの秒・上位 10 module・回の比・mode と load の帯ごとの比 → `by-day.json`。
- `rounds.py`: 再現の nextest のログ（`LOGDIR/<r|q><周>-<設定>.log`）から周ごとの壁時計と 1 本あたりの中央値・p90 → `repro-commits.json`・`repro-qos.json`。
- `rounds.txt`: 再現の周ごとの開始時刻・load・nextest の Summary の行。
- mode の切り替わりは `dagq events --full --kind supervisor_started --since 2026-10-08T10:45:00Z --until 2026-10-08T12:00:00Z`、回の rebase 先は同じ時間の `integration_started` の `main` で読んだ。

区分: JSON と `rounds.txt` は、ログと再現の nextest の出力から上のコマンドで作り直せるビューで、EVENTS.json と再現のログは消えてよい材料。host の run の directory のログが消えると作り直せないので、JSON を正として残す。
