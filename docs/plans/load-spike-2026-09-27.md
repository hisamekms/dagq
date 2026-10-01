---
id: plan-load-spike-2026-09-27
type: plan
title: 2026-09-27 04:00〜06:30 JSTのloadの山とcmuxのcaptureの時間切れの出どころ
status: completed
created: 2026-09-29
updated: 2026-09-30
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-nextest-test-threads
  - plan-nextest-measurement
---

# 2026-09-27 04:00〜06:30 JSTのloadの山とcmuxのcaptureの時間切れの出どころ

task 563が[nextest-test-threads](nextest-test-threads.md)の7章で、並列度8の期間の`backend_call_failed` 35件のうち21件がintegrateのllvm-covの段の外で起き、2026-09-27の04:00〜04:02・05:39〜05:41・06:18〜06:24 JSTにload1 30〜57で固まっていたと書いた。task 932として、その山の出どころを`metrics.csv`・本番queueのevent・workerのsessionの記録から調べた。この文書は値を変えずに残す調査の記録で、打ち手はtaskに登録せず、receiptのfollow_upsに書いた。

## 要点

- **3つの山はどれもintegrateのllvm-covの段の外で、workerが手元で流した重いtestが2〜3本重なった時間だった**。04:00〜04:02はtask 619のworkerの`it`のほぼ全体（`-- runtime_ lifecycle_ …`）とtask 522のworkerのe2e・`runtime_integrate::`、05:39〜05:41はtask 432のresumeのworkerの手元の`cargo llvm-cov nextest`（全976件）とtask 314のworkerのe2eの繰り返しとtask 221のworkerの`--lib`・`--test it --test plugin`（全体）、06:18〜06:24はtask 418のresumeのworkerの手元の`cargo llvm-cov nextest`（全992件）とtask 442のworkerの10 moduleの`it`とtask 474のworkerのclippy・test・e2eが重なっていた。
- **hostはCPUで飽和していた**。山の間は`cpu_idle`がほぼ0で、`cpu_sys`が約40%あった。swapは増えず（1554MB一定）、pageoutも少ないので、memoryの不足ではない。
- **CPUの大半は`metrics.csv`の分類に入らないprocessが使っていた**。山の間のbusyな約8コアのうち6〜7コアは、`claude`・`rust`・`test`・`cmux`のどの列にも入らない。testのbinary（`target/*/deps/…`）が起動する`target/debug/dagq`・git・sh・stubのscriptは`test`の列に数えられないので、testの子processと見る（sample.shの分類から来る推定で、processごとの記録は無い）。
- **21件の内訳**: 04:00〜04:02に6件、05:39〜05:41に4件、06:18〜06:24に6件、3つの山の外に5件（03:20・04:57の2件は同じ形の小さな山、08:15の1件はload 1.2、10:14の2件はcmuxの`TabManager not available`で時間切れではない）。
- **testの並列度との関係**: 関係する。ただし効いているのはintegrateの`NEXTEST_TEST_THREADS`だけではなく、`[run.env]`がworkerにも渡す`RUST_TEST_THREADS=8`と`NEXTEST_TEST_THREADS=8`で、workerの手元の`cargo test`（1 binaryの中の8 thread）と手元の`cargo llvm-cov nextest`（8 process）がどちらも8並列で走る。重いtestが同時に2〜3本走ると、testが16〜24本同時に子processを起こす。主因は並列度8そのものより、workerの手元のtestの範囲が広いこと（全体か全体に近い範囲、手元のllvm-cov、e2eの繰り返し）と、それが重なることにある。
- **特定できなかった部分**: processごとのCPUの内訳（どのprocessが6〜7コアを使ったか）は記録が無く特定できない。XprotectService・Spotlight・Time Machineなどhostのprocessは、統一ログに山の時間の記録が無く（Time Machineはdestinationをmountできず動いていない）、主因とは見ないが、CPUの記録が無いので否定もしきれない。

## 1. 方法

| 材料 | 使い方 |
| --- | --- |
| `~/.local/share/dagq-hostmetrics/metrics.csv` | 約30秒ごとの`load1`・`cpu_user`/`cpu_sys`/`cpu_idle`・memory・swap・pageout・`runs`と、`claude`・`rust`（rustc・cargo・clippy-driver・sccache・ld）・`test`（commが`target/…/deps/`のprocess）・`cmux`のprocess数とCPU（psの%、100で1コア）。時刻はJST |
| `dagq events --all --full --since/--until`（固定バイナリ） | 2026-09-26T18:00Z〜22:00Zの1804件と、期間全体の`backend_call_failed`・`verification_command`。eventの時刻はUTCで、9時間足してJSTにした |
| workerのClaude Codeのsessionの記録（`~/.claude/projects/*-runs-<run>-worktree/*.jsonl`） | Bashのtool_useとtool_resultの時刻から、workerが手元で流した`cargo test`・`cargo nextest`・`cargo llvm-cov`・e2eの開始と終了を取った（backgroundのものは`task-notification`の時刻を終わりとした） |
| macOSの統一ログ（`log show`） | 山の時間のhostのprocess（backupd・mds_stores・mdworker_shared・XprotectService・softwareupdated・mediaanalysisd）の記録の有無 |
| `sampler.log` | 空で、使える記録は無かった |

llvm-covの段の区間は、`verification_command` eventの`created_at`から`duration_secs`を引いた時刻から`created_at`までとした（nextest-measurementの6章と同じ）。表の「未分類」は`(cpu_user + cpu_sys) × 8 ÷ 100`（busyなコア数）から4つの列のCPUの合計を引いたコア数である。

## 2. 21件の振り分け

期間（event 15046〜21344）の`backend_call_failed` 35件のうち、llvm-covの段と重ならない21件。「同時の重いtest」は、そのeventの前後30秒に走っていたworkerの手元のコマンド（run IDの先頭8文字: 種類）で、`LLVMCOV`は手元の`cargo llvm-cov nextest`、`E2E`は`--test e2e`、`IT`は`--test it`、`LIB`は`--lib`。

| event | 時刻（JST） | op | 試行 | load | 山 | 同時の重いtest |
| --- | --- | --- | --- | --- | --- | --- |
| 17939 | 03:20:30 | capture | 1/3 | 42.8 | 山の外（03:17〜03:23の小さな山） | 3888fd0e: LLVMCOV、45ed4489: E2E |
| 18323 | 04:00:52 | exists | 1/3 | 32.8 | 1（04:00〜04:02） | 189448eb: IT、a71e73a8: IT |
| 18324 | 04:01:24 | capture | 1/3 | 41.8 | 1 | 189448eb: IT、a71e73a8: IT |
| 18325 | 04:01:43 | capture | 2/3 | 46.6 | 1 | 189448eb: IT・E2E、a71e73a8: IT |
| 18326 | 04:01:58 | capture | 3/3（exhausted） | 54.8 | 1 | 189448eb: IT・E2E、a71e73a8: IT |
| 18327 | 04:02:16 | capture | 1/3 | 57.6 | 1 | 189448eb: E2E、a71e73a8: IT |
| 18328 | 04:02:35 | capture | 2/3 | 52.0 | 1 | 189448eb: E2E、a71e73a8: IT |
| 18688 | 04:57:30 | exists | 1/3 | 30.1 | 山の外（04:56〜04:58の小さな山） | a5da06dc: E2E、f2f7d7ae: IT |
| 18910 | 05:39:51 | capture | 1/3 | 20.9 | 2（05:39〜05:41） | 4c10e12a: LLVMCOV、6fa147a0: LIB、8a1b2198: E2E |
| 18912 | 05:41:12 | capture | 1/3 | 32.5 | 2 | 4c10e12a: LLVMCOV、6fa147a0: IT・LIB、8a1b2198: E2E |
| 18913 | 05:41:31 | capture | 2/3 | 35.1 | 2 | 同上 |
| 18914 | 05:41:47 | capture | 3/3（exhausted） | 34.6 | 2 | 同上 |
| 19077 | 06:18:21 | exists | 1/3 | 26.5 | 3（06:18〜06:24） | 3888fd0e: LLVMCOV、4ff154ca: IT |
| 19078 | 06:18:57 | capture | 1/3 | 31.1 | 3 | 3888fd0e: LLVMCOV、4ff154ca: IT・LIB |
| 19079 | 06:19:17 | capture | 2/3 | 35.3 | 3 | 同上 |
| 19085 | 06:23:15 | capture | 1/3 | 29.1 | 3 | 09f14674: IT、3888fd0e: LLVMCOV |
| 19086 | 06:23:34 | capture | 2/3 | 34.4 | 3 | 同上 |
| 19087 | 06:24:11 | exists | 1/3 | 39.8 | 3 | 同上 |
| 19944 | 08:15:06 | exists | 1/3 | 1.2 | 山の外（loadは低い） | なし |
| 20939 | 10:14:27 | exists | 1/3（`backend_failed`） | 12.1 | 山の外 | 07f89a1d: E2E、d536f89a: E2E |
| 20941 | 10:14:53 | exists | 1/3（`backend_failed`） | 9.8 | 山の外 | 07f89a1d: E2E |

- 表の「同時の重いtest」は、sessionの記録で終わりの時刻が取れたコマンドだけを並べた。18323・18324のときも、189448ebのbackgroundのe2e（03:59:49に開始）は走っていたと見られる（3.1節）。
- 山1が6件、山2が4件、山3が6件で、3つの山で16件。captureの時間切れで段の外の14件のうち、山の外は17939の1件だけ。
- 20939・20941は`code: backend_failed`で、cmuxが`unavailable: TabManager not available`を返したもの（時間切れではない）。19944はload 1.2で起きた`exists`の時間切れで、loadでは説明できない。この3件は3つの山と別の原因で、この文書では調べていない。
- 17939と18688は、山と同じ形（workerの手元の重いtestが2本重なり、load 30〜43）の小さな山で起きた。

## 3. 時間帯ごとの推移と内訳

### 3.1 山1: 04:00〜04:02

load1（`metrics.csv`、前後30分の03:30〜04:32）: 平均12.6、中央値10.5、p90 22.9、最大56.9（04:02:07）。山の5分（03:59〜04:04）は平均31.7、`cpu_idle`の中央値1%。swapは1554MBで変わらない。

| 時刻 | load1 | user/sys/idle % | claude 数/CPU | rust 数/CPU | test 数/CPU | cmux CPU | busyなコア | 未分類のコア |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 03:59:24 | 8.3 | 50/48/2 | 6/14 | 3/0 | 2/158 | 23 | 7.8 | 5.9 |
| 03:59:56 | 11.6 | 59/41/1 | 6/32 | 5/74 | 1/78 | 6 | 7.9 | 6.0 |
| 04:00:29 | 20.2 | 62/38/0 | 6/32 | 4/0 | 2/36 | 27 | 8.0 | 7.0 |
| 04:01:02 | 37.3 | 61/39/1 | 6/11 | 10/114 | 2/66 | 10 | 8.0 | 5.9 |
| 04:01:34 | 44.6 | 59/41/0 | 6/12 | 8/52 | 1/44 | 4 | 8.0 | 6.9 |
| 04:02:07 | 56.9 | 61/39/0 | 6/12 | 5/48 | 1/83 | 1 | 8.0 | 6.6 |
| 04:02:40 | 50.1 | 58/42/0 | 6/10 | 6/18 | 2/72 | 1 | 8.0 | 7.0 |
| 04:03:12 | 35.3 | 13/11/76 | 6/4 | 3/0 | 2/8 | 5 | 1.9 | 1.7 |
| 04:04:15 | 13.6 | 18/10/72 | 6/17 | 3/0 | 2/7 | 16 | 2.2 | 1.8 |

`test`の列が1〜2（`it`か`e2e`のbinaryが1〜2本）で、integrateのnextestの形（`test`が8〜9）ではない。04:03にCPUが空いた後もload1が下がりきらないのは、1分の移動平均の遅れである。

動いていたrunと工程（`--parallel 3`、slotは3つとも埋まっていた）:

| run（task） | 状態と根拠のevent | 04:00〜04:05に手元で流していたもの |
| --- | --- | --- |
| a71e73a8（619） | worker。`run_claimed` 18233（03:48:25）、receiptは18472（04:32:31） | 03:59:02〜04:04:55 `cargo test --locked --test it -- runtime_ lifecycle_ landing_branch:: plan_review:: location::`（`it`のほぼ全体を1 binary・8 threadで） |
| 189448eb（522） | worker。`run_claimed` 18291（03:54:41）、receiptは18333（04:10:08） | 03:59:49〜約04:02:05 e2e（background。`auto_update_…`のtestがabortした）、04:01:11〜04:02:05 `--test it runtime_integrate::`（27件）、04:02:10〜04:05:50 e2eを流し直し（`happy_path`・`two_independent`が失敗） |
| 2ea99280（570） | worker。`run_claimed` 18310（03:58:17） | 編集だけ（04:04以降に`runtime_candidates::`など小さなtest） |
| integrate | 8af54e2e（431）のllvm-covの段は03:54:54〜03:58:09（event 18301）で、189448ebの段は04:11:02〜04:17:12（18370）。山の間は無い | — |
| headless job | runtimeのplanner 223（`draft_planner_opened` 18309、03:58:15）が開いていた。review・plan review・observerは無い | — |

supervisorは04:00:19に`claim_held`（load 16.2、event 18322）で新しいclaimを止めていたが、走っているrunの手元のtestは止まらない。

### 3.2 山2: 05:39〜05:41

load1（05:09〜06:12）: 平均11.3、中央値9.2、p90 23.3、最大34.6（05:41:51）。山の5分（05:38〜05:43）は平均20.5、`cpu_idle`の中央値0%。

| 時刻 | load1 | user/sys/idle % | claude 数/CPU | rust 数/CPU | test 数/CPU | cmux CPU | busyなコア | 未分類のコア |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 05:38:34 | 9.0 | 92/9/0 | 7/10 | 17/343 | 1/2 | 2 | 8.0 | 4.5 |
| 05:39:06 | 12.4 | 55/36/8 | 7/18 | 4/0 | 9/71 | 41 | 7.3 | 6.0 |
| 05:39:40 | 15.8 | 59/41/0 | 7/12 | 14/123 | 9/70 | 0 | 8.0 | 5.9 |
| 05:40:13 | 22.3 | 53/46/0 | 7/7 | 7/41 | 9/74 | 8 | 8.0 | 6.7 |
| 05:40:45 | 24.6 | 58/41/1 | 7/20 | 7/25 | 9/68 | 0 | 8.0 | 6.8 |
| 05:41:18 | 32.5 | 57/43/0 | 7/14 | 5/0 | 9/54 | 18 | 8.0 | 7.1 |
| 05:41:51 | 34.6 | 60/39/1 | 7/18 | 6/57 | 5/32 | 38 | 8.0 | 6.5 |
| 05:42:23 | 23.4 | 64/35/1 | 7/12 | 5/0 | 3/22 | 25 | 8.0 | 7.4 |
| 05:43:28 | 19.1 | 63/37/0 | 7/13 | 4/0 | 2/95 | 27 | 8.0 | 6.6 |

`test`の9はnextestの8 process（とe2eのbinary）の形だが、この時間にintegrateのllvm-covは走っていない（4c10e12aのworkerの手元のllvm-cov）。

| run（task） | 状態と根拠のevent | 05:36〜05:46に手元で流していたもの |
| --- | --- | --- |
| 4c10e12a（432） | resume。integrateのllvm-covの失敗（event 18878、`exit=100`、段702秒）で`resume_started` 18885（05:35:28） | 05:36:34〜05:38:02 `cargo nextest run --test it runtime_repair::`を5回、05:38:13〜05:43:48 `cargo llvm-cov nextest --locked --fail-under-lines 80`（976件、`Summary` 279.4秒） |
| 8a1b2198（314） | worker（`tests/e2e.rs`を変えるtask）。`run_claimed` 18901（05:35:33） | 05:36:27〜05:40:55 e2eを3回続けて、05:41:09〜05:41:32 もう1回、05:42:15〜05:50:59 さらに3回 |
| 6fa147a0（221） | worker。`run_claimed` 18855（05:23:32） | 05:39:59〜05:41:28 `cargo test --locked --lib`、05:41:39〜05:45:55 `cargo test --locked --test it --test plugin`（`it`の全体、background） |
| integrate | b5325347（473、docs）が05:35:27〜05:35:29、4c10e12aの2回目が05:45:01から（llvm-covの段は05:45:38〜05:49:51、event 18935）。山の間は無い | — |
| headless job | observerは05:32:04〜05:33:13（18869・18875）で山の前に終わっていた。review・plan reviewは無い | — |

4c10e12aの手元のllvm-covは、integrateの検証が落ちてresumeされたrunの再現で、AGENTS.mdが許す例外に当たる。

### 3.3 山3: 06:18〜06:24

load1（05:48〜06:55）: 平均14.9、中央値11.3、p90 32.6、最大42.6（06:24:41）。山の9分（06:17〜06:26）は平均25.8、`cpu_idle`の中央値2%。

| 時刻 | load1 | user/sys/idle % | claude 数/CPU | rust 数/CPU | test 数/CPU | cmux CPU | busyなコア | 未分類のコア |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 06:16:59 | 5.8 | 41/20/39 | 7/11 | 4/4 | 8/14 | 1 | 4.9 | 4.6 |
| 06:18:03 | 16.8 | 80/20/0 | 7/9 | 18/99 | 8/58 | 0 | 8.0 | 6.3 |
| 06:18:36 | 27.8 | 62/38/0 | 7/11 | 12/110 | 8/64 | 0 | 8.0 | 6.1 |
| 06:19:08 | 35.4 | 61/39/0 | 7/9 | 7/36 | 9/59 | 0 | 8.0 | 7.0 |
| 06:20:13 | 21.5 | 43/24/34 | 7/17 | 6/85 | 2/16 | 5 | 5.4 | 4.1 |
| 06:21:16 | 16.1 | 52/26/21 | 7/88 | 7/89 | 9/29 | 44 | 6.3 | 3.8 |
| 06:22:52 | 17.7 | 53/48/1 | 7/9 | 4/0 | 8/87 | 0 | 8.0 | 7.1 |
| 06:23:30 | 33.3 | 56/44/0 | 7/8 | 4/0 | 9/96 | 0 | 8.0 | 7.0 |
| 06:24:07 | 40.7 | 54/46/1 | 7/12 | 4/0 | 9/180 | 0 | 8.0 | 6.0 |
| 06:24:41 | 42.6 | 58/41/0 | 7/16 | 7/22 | 3/56 | 0 | 8.0 | 7.0 |
| 06:25:48 | 33.6 | 65/24/11 | 7/12 | 6/88 | 2/10 | 112 | 7.1 | 4.9 |
| 06:26:51 | 15.0 | 17/7/77 | 7/7 | 3/0 | 2/4 | 6 | 1.9 | 1.7 |

| run（task） | 状態と根拠のevent | 06:16〜06:28に手元で流していたもの |
| --- | --- | --- |
| 3888fd0e（418） | resume。task 221の着地の後のrebaseの衝突（`resume_started` 19071、06:14:27、`counted: false`）で、integrateの検証の失敗ではない | 06:16:03〜06:20:02 `cargo test --test plugin`と`cargo llvm-cov nextest`、06:20:20〜06:24:53 `cargo llvm-cov nextest`をもう一度（992件、`Summary` 217.8秒）、06:24:56〜06:27:05 e2e |
| 09f14674（442） | worker。`run_claimed` 19020（06:03:51） | 06:20:40〜06:21:44 `--test it runtime_stall::`、06:22:48〜06:27:48 `runtime_resume`など10 module（145件）を1つずつ。この中で`runtime_resume`の2件と`runtime_adopt`の1件が落ちた |
| 4ff154ca（474） | worker。`run_claimed` 19041（06:07:22） | 06:17:52〜06:19:29 clippy・`cli_roles::`・`cli_read::`・`--test plugin`・`--lib`、06:20:11〜06:21:44 clippyとe2e |
| aefa7b15（433） | receiptの後の`approve_landing`のask（event 19068、06:14:25）を待つ。手元の処理は無い | — |
| integrate | 6fa147a0（221）のllvm-covの段は06:03:45〜06:07:13（19030）で、4ff154caの段は06:27:52〜06:33:21（19236）。山の間は無い | — |
| headless job | 山の間は無い。06:24:37からrunのfollow_upのruntimeのplannerとplan reviewが続けて開き（19088〜）、observerは06:33:15から | — |

3888fd0eのresumeはrebaseの衝突によるもので、手元のllvm-covはAGENTS.mdの例外（integrateの検証が落ちてresumeされたrun）に当たらない。同じrunの1回目のresume（event 17918、検証の失敗）で手元のllvm-covを流していて（03:16〜03:21、17939の小さな山）、3回目のresumeでも同じことをしたと見える。

## 4. 見立て

### 4.1 主因

主因は、**workerが手元で流した範囲の広いtestが2〜3本同時に走ったこと**と見る。確からしさは高い。

- 3つの山の全16件で、integrateのllvm-covの段は走っておらず、代わりにworkerの手元の重いtest（`it`の全体か全体に近い範囲、手元の`cargo llvm-cov nextest`、e2e）が2〜3本重なっていた（2章・3章）。山の外の2件（17939・18688）も同じ形だった。
- 期間全体（`metrics.csv`の1433標本）を、integrateのllvm-covの段の有無と、同時に走っていたworkerの手元の重いtest（`LLVMCOV`・`E2E`・`IT`）の本数で分けると、load1は本数とともに上がる。

  | integrateのllvm-cov | workerの手元の重いtest | 標本 | load1 中央値 | p90 | 最大 | 30以上 |
  | --- | --- | --- | --- | --- | --- | --- |
  | なし | 0本 | 422 | 3.6 | 11.6 | 26.2 | 0 |
  | なし | 1本 | 361 | 8.5 | 21.2 | 56.9 | 6 |
  | なし | 2本 | 131 | 10.9 | 26.9 | 50.1 | 10 |
  | なし | 3本 | 9 | 17.8 | 26.8 | 26.8 | 0 |
  | あり | 0本 | 336 | 11.6 | 22.5 | 40.5 | 12 |
  | あり | 1本 | 139 | 15.8 | 30.7 | 46.6 | 14 |
  | あり | 2本 | 35 | 22.0 | 35.5 | 45.1 | 8 |

  workerの手元のtestの区間はsessionの記録から取ったもので、`IT`には1 moduleだけの軽いものも入る。backgroundのコマンドで終わりの記録が無いものは数えていない。
- 山の間はbusyなコアが8.0で、うち6〜7コアが分類に入らないprocessだった。`test`の列のbinaryは多くて1〜2コアしか使っておらず、testが起動する子process（`target/debug/dagq`、git、sh、stubのscript）が大半を使ったと見る。`cpu_sys`が約40%と高いことも、processの起動とファイル操作が多いtestの形に合う。
- memoryは原因ではない。swapは期間を通じて1554MBで変わらず、山の間のpageoutは数分で数百〜千程度。

cmuxの時間切れは、CPUが飽和して、cmuxのCLIと本体の応答が30秒の上限に間に合わなかったものと見る。山の間の`cmux`の列のCPUは0〜40%と低く、cmux自体が重かったのではなく、CPUを取れなかった形である（推定）。e2eは本物のcmuxにworkspaceを作るので、cmuxへの要求を増やしたかもしれないが、山3の前半（06:18〜06:19）の3件のときe2eは走っておらず、e2eは必要条件ではない。

### 4.2 testの並列度（8）との関係

関係する。`[run.env]`の`RUST_TEST_THREADS=8`と`NEXTEST_TEST_THREADS=8`は、integrateの検証だけでなくworkerのworkspaceにも渡る。そのため、workerの手元の`cargo test --test it`は1 binaryの中で8本、手元の`cargo llvm-cov nextest`とstressは8 processを同時に走らせ、e2eも8 threadで複数のsupervisorとcmuxのworkspaceを同時に動かす。重いtestが2〜3本重なると、同時のtestは16〜24本になる。4から8に上げたことが、1本あたりの負荷を上げたのは確かである。

ただし、次の点から、並列度8だけを主因とは見ない。

- 同じ8の設定でも、integrateのllvm-covの段だけが走っている間のload1の中央値は11.6で、30以上になったのは336標本のうち12（4%）だった。山を作ったのは、workerの手元のtestが重なったことである。
- 山の中のworkerの手元のtestの多くは、AGENTS.mdの「変更に関係するtestだけ」より広い。a71e73a8の`-- runtime_ lifecycle_ …`（`it`の大半）、6fa147a0の`--test it --test plugin`（全体）、3888fd0eのrebaseの衝突のresumeでの手元のllvm-cov、8a1b2198のe2eの計7回の繰り返しである。
- supervisorの`--max-load 16`（`claim_held`）は新しいclaimを止めるだけで、走っているrunの手元のtestは止めない。山はどれも、claimが止まった後に、すでにslotにいたrunが作った。

6に下げた（task 930）後もこの形が残れば、1本あたりの負荷は下がるが、重なりは減らない。6の期間の測り直し（nextest-test-threadsの8.2節）では、段の外のloadの山を、workerの手元のtestの重なりと分けて読むのがよい。

### 4.3 特定できなかった部分

- **processごとのCPU**: `metrics.csv`は4つの分類の合計しか持たず、`sampler.log`は空で、processごとの記録は無い。未分類の6〜7コアがtestの子processだというのは、sample.shの分類と時刻の一致からの推定で、どのprocess（`dagq`・git・sh・stub）が何コアかは特定できない。
- **hostのprocess**: 統一ログでは、3つの山の時間にbackupd・mds_stores・mdworker_shared・XprotectService・softwareupdatedの記録は無かった。Time Machineは`tmutil latestbackup`が「Failed to mount destination」を返し、動いていない。mediaanalysisdとcontainermanagerdは山の時間にも静かな時間（05:28〜05:34、load1 平均2.3）にも同じくらいの件数のログを出していて、山と相関しない。ただし、ログの件数はCPUの使用量ではないので、hostのprocessが寄与しなかったとは言い切れない。
- **山の外の3件**: 19944（load 1.2での`exists`の時間切れ）と20939・20941（cmuxの`TabManager not available`）は、loadの山と別の原因で、調べていない。
- **cmuxの内部**: cmuxが時間切れのときに何を待っていたかは、cmuxの側の記録が無く分からない。

## 5. 打ち手の候補

どれもtaskには登録せず、task 932のreceiptのfollow_upsに書いた。

1. **workerの手元のtestの範囲を守らせる**: 「変更に関係するtestだけ」の目安に、広いfilter（`runtime_`・`lifecycle_`のような接頭辞だけのもの）や`--test it`の全体を流さないことと、手元の`cargo llvm-cov`はintegrateの検証の失敗によるresumeだけに限る（rebaseの衝突のresumeでは流さない）ことを、runtimeのworkerのprompt（とresumeのprompt）に書く。
2. **workerの手元のtestの並列度を分ける**: `[run.env]`の`RUST_TEST_THREADS`・`NEXTEST_TEST_THREADS`はintegrateとworkerの両方に効く。workerの手元の分だけを小さくできる形（integrateの検証のenvだけに並列度を渡すなど）を検討する。
3. **loadが高いときにworkerの重いtestを待たせる**: `claim_held`はclaimを止めるだけなので、load1が閾値を超えている間は、workerのe2eや手元のllvm-covを始める前に待つ仕組み（hostの共有のsemaphoreや、promptでの指示）を検討する。一方、AGENTS.mdのstressの項は「負荷が下がるのを待たない」としているので、stressとは分けて決める必要がある。
4. **e2eの繰り返しを1本ずつにする**: `tests/e2e.rs`を変えるtaskでe2eを何回も続けて流すと、本物のcmuxとCPUを長く使う。繰り返すときの回数とtest threadの数の目安を決める。
5. **`metrics.csv`にtestの子processを数える**: sample.shの分類に、`target/*/debug/dagq`と`llvm-cov-target`のbinary、git、shを足すか、loadが閾値を超えたときに`ps`の上位を`sampler.log`に残す。今回特定できなかった内訳が次は取れる（hostの作業なので`ops`）。
6. **6の期間の測り直しで分けて読む**: nextest-test-threadsの8.2節の測り直しで、`backend_call_failed`とload1を、integrateの段の中・workerの手元の重いtestの重なりの中・どちらでもないの3つに分けて数える。

## 6. 並列度6の期間を1030の着地で分けた（task 1034）

### 6.1 範囲と数え方

**1030の後も、integrateのllvm-covの外でworkerの重いtestと重なる山は残った**。ただし、この表は時刻の重なりであり、workerだけがそのloadを作ったという因果の判定ではない。既存の1〜5章の数字は変更していない。

| 境界・条件 | 今回の値（時刻はすべてUTC） |
| --- | --- |
| 始まり（含む） | event **39197**、2026-09-28T20:26:41.048Z。task 930の`run_env_changed` |
| 前後の境界 | event **45410**、2026-09-29T18:24:34.798Z。task 1030の`run_integrated`、commit `32f5102dca644d33a72ff5656f23bea60b1960e0` |
| 終わり（含む） | event **49728**、2026-09-30T06:20:47.739Z。取得時のcursorを固定。以後のeventは含めない |
| 時間と着地数 | 前21.965時間、後11.937時間。1030自身を除く着地は前55、後**50 run**（後の最後の着地は49353）。「後10 run以上」を満たす |
| `--parallel` | 3。期間内の47件の`supervisor_started`と121件の`run_claimed`、219件の`backend_call_failed`で確認 |
| `[run.env]` | 始まりで`RUST_TEST_THREADS`と`NEXTEST_TEST_THREADS`が8→6。以後の`run_env_changed`は0件。`CARGO_BUILD_JOBS=4`、sccacheの設定は継続 |
| toolchain・provider | `run_claimed`のRustは全121件で1.98.1 / aarch64-apple-darwin、Codexは0.155.1。Rustの変更は観測されない。Claudeは2.1.283を基本に、event 42983（09-29 09:03:35.040）の1件が2.1.284、次のclaim 43118（13:33:30.626）は2.1.283。claim間の未観測の切替時刻は断定しない。後にはCodex workerも含む |

本番queueには固定バイナリの読み取り専用の`events`だけを使った。全10532 eventを次の取得結果から49728までに固定した（`--after`は指定IDを含まない）。task 932・1030のreceiptも読んだ。

```sh
~/.local/bin/dagq events --after 39196 --all --full --limit 100000
```

load1はqueueの`host/metrics-20260928.csv`・`metrics-20260929.csv`・`metrics-20260930.csv`の`unix`と`load1`を使い、上の時刻窓にある数値の行2544標本を数えた。外部の`~/.local/share/dagq-hostmetrics`は使わず、変更もしていない。平均は標本の算術平均、中央値は標本の中央値で、時間で重みを付けず、欠測は埋めない。最大の欠測は09-29 04:16:00〜09:03:24の17244秒、次は09-30 03:31:36〜05:13:52の6136秒。したがって「30以上の数」は時間の長さでも期間全体の山の総数でもない。

分類は次の排他的な3つ（境界は開始・終了とも含む）で、前後は1030の着地時刻で分ける。

- **(a)** integrateのllvm-covの区間。`phase=integration`の`verification_command`97件の`created_at - duration_secs`〜`created_at`。失敗した試行も含む。workerと重なったときも(a)を優先する。
- **(b)** (a)の外で、workerの手元の重いtestの区間と重なるもの。Claude Codeのsession JSONLのBash `tool_use`〜対応する`tool_result`、backgroundは`task-notification`の時刻までとした（4.1節と同じ方法）。Codexの8区間はrollout JSONLの`exec_command`〜同じsession IDの`write_stdin`が終了を返す記録で補った。複合shellは分解できないため、前後のbuild・clippy・小さなtestも含めたtool呼び出し全体の区間である。
- **(c)** (a)(b)のいずれにも入らないもの。終了時刻不明のworker、少数moduleのtest・stress、build、runtimeの自動更新のe2e、hostのほかの仕事もここに入りうる。「testが無かった」という意味ではない。

(b)の種類は`E2E`（`--test e2e`、名前で絞った実行も含む）、`LLVMCOV`（手元のllvm-cov、計測用scriptの呼び出しも含む）、`IT-wide`とした。`IT-wide`は無filter、`runtime_`・`lifecycle_`・`cli_`などmoduleの境を持たない広いprefix、または1つの呼び出しで**8 module以上**を並べるもの（loopを含む）。8は今回の集計上の線引きで、許容するtest範囲の規則ではない。1 moduleも数えた4.1節の表とはこの点が違う。単なる引用・receiptやscriptの定義だけ・`--version`・`--no-run`・testが0件の呼び出し・承認拒否は実行区間に入れない。少数testのstressは高負荷でもこの3種類には足さない。

全区間を[区間一覧CSV](load-spike-1034-intervals.csv)に残した。`run_id`は省略なし、`task_id`、UTCの開始・終了、コマンドの種類と抜粋、根拠のevent IDまたはtool call IDを持つ。`group=worker_end_missing`の22件（前18・後4）はbackgroundの終了が記録から取れず、4.1節と同じく集計から除外した。未観測の区間は(c)に残りうるので、(b)は捕捉できた区間の値である。一方、複合shellや通知の遅れは(b)を長く見積もりうる。

### 6.2 load1とbackendの失敗

`backend_call_failed`はretryの試行ごとに1件で、全部が`code=backend_timeout`（ほかのcodeは0件）だった。`exhausted`は`stats`と同じく`retry_after_ms`がnullまたは欠落のeventであり、`attempt == max_attempts`だけでは判定しない。たとえば39663の`send_exit`は1/3でもretryなしなのでexhaustedである。「retryあり」は次の試行を予定した件数で、後で回復したことの保証ではない。

| 1030 | 区分 | load標本 | 平均 | 中央値 | 最大 | 30以上 | backend失敗 | うちexhausted |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 前 | (a) | 549 | 17.18 | 13.69 | 59.27 | 83 | 112 | 35 |
| 前 | (b) | 168 | 12.46 | 8.68 | 39.59 | 14 | 5 | 0 |
| 前 | (c) | 931 | 7.64 | 2.22 | 80.02 | 52 | 52 | 23 |
| 後 | (a) | 546 | 13.80 | 12.77 | 73.00 | 8 | 15 | 3 |
| 後 | (b) | 74 | 15.16 | 9.40 | 123.21 | 8 | 18 | 9 |
| 後 | (c) | 276 | 11.62 | 6.64 | 283.62 | 16 | 17 | 6 |

(a)にはworkerと重なる標本が前137・後118ある。この重なりを(b)にも足してはいない。前後の時間も標本数も異なるので、件数だけを1030の効果の大小とは読まない。

op別の内訳。各セルは **retryあり / exhausted**、codeは全セルで`backend_timeout`。opのない組み合わせは0/0。

| op | 前(a) | 前(b) | 前(c) | 後(a) | 後(b) | 後(c) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `capture` | 51 / 12 | 4 / 0 | 22 / 8 | 8 / 1 | 8 / 3 | 9 / 3 |
| `create` | 0 / 0 | 0 / 0 | 0 / 1 | 0 / 0 | 0 / 0 | 0 / 0 |
| `exists` | 26 / 9 | 1 / 0 | 7 / 2 | 4 / 0 | 1 / 0 | 2 / 0 |
| `listed_workspace_ids` | 0 / 13 | 0 / 0 | 0 / 11 | 0 / 2 | 0 / 6 | 0 / 3 |
| `send_exit` | 0 / 1 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| `workspaces_described` | 0 / 0 | 0 / 0 | 0 / 1 | 0 / 0 | 0 / 0 | 0 / 0 |

照合: 前169件（retryあり111 / exhausted58）、後50件（32 / 18）、全体219件（143 / 76）。load標本は前1648・後896で2544件。どちらも3区分の和が窓全体と一致する。

### 6.3 workerの区間の内訳と1030後の山

CSVの開始時刻で前後を分けたtool呼び出しの件数（cargoの起動回数ではない）。境界をまたぐ呼び出しは無かった。各行は(a)との重なりを除く前のworker区間で、負荷の表ではその重なりを(a)へ渡す。

| コマンドの種類 | 前 | 後 |
| --- | ---: | ---: |
| E2E | 76 | 42 |
| IT-wide | 15 | 5 |
| LLVMCOV | 3 | 1 |
| E2E+IT-wide | 3 | 1 |

終了が取れたのは計146区間、65 run。複合の`E2E+IT-wide`は1区間として数えた。全runのIDと全開始・終了はCSVを参照。1030後の(b)の30以上8標本とexhausted9件は、次の区間に集中した。

| run（task） | UTCの区間 | コマンド | (b)の30以上 / 最大 | (b)のexhausted |
| --- | --- | --- | --- | --- |
| `2d1f5abd-0c40-4cd7-8073-9f0ce058ae9a`（1050） | 09-30 01:06:09.083〜01:09:38.283 | `cargo test --locked --test it $m::`、`runtime_stall`から`runtime_headless`まで11 moduleのloop（104 test）。同runは後で13 moduleのloopを2回実行 | 3 / 44.62 | 5（capture 3、listed_workspace_ids 2）。event 48395・48398・48399・48401・48404 |
| `1930d5ff-17cc-4ef0-b1fd-efb9fd550920`（1079） | 09-30 05:48:36.466〜06:03:16.669 | `cargo test --locked --test e2e -- --ignored` | 5 / 123.21 | 4（listed_workspace_ids）。event 49559〜49562 |
| `0f073ce0-14c0-4a45-9f0d-2b7fd34de559`（1022） | 09-30 05:51:07.220〜05:57:29.866 | 少数testのstress・fmtの後に全体e2eを実行する複合shell | 上の5のうち2 / 123.21 | 上の4と同じevent（重複計上しない） |

後の手元のllvm-covはrun `314a9d7b-dae7-4d17-8afe-bfeebbb436a8`（1162）の09-30 00:18:03.070〜00:28:17.169だった。47950の`resume_started`は`escalation_reason=verification_failed`で、1030が残した再現の例外に当たる。重い区間があることだけで規則違反とはしない。同様に上の11 moduleのloopは測定の`IT-wide`に入るが、この計測だけでは個々のmoduleの必要性までは裁定しない。

### 6.4 見立てと次の判断

**1030後も(b)にload1 30以上とexhaustedの両方が残るため、取り下げた1031・1032を再検討する材料はある**。workerの広いtest範囲の明確化だけで段の外の山が消えた、とは言えない。全体e2eが2 runで重なる区間もある。手元のtestの並列度をintegrateから分ける案（1031）と、重いtestを始める時点で調整する案（1032）をreceiptの`decision`に残す。taskの登録・設定の変更はしていない。

ただし、この結果から「直ちにworkerの並列度を4にする」「高loadでは全testを待たせる」とは決めない。

- 後の01:07前後には別run `c6ef5297`（1058）が、少数testのstressと12本の`yes`で負荷を作るコマンドを01:04:12.125に開始していた。これは(b)の定義外で、backgroundの終了通知も取れなかった。1050の広いtestだけに山を帰属させられない。
- 後の最大283.62（09-30 06:07:57）は(c)である。(b)の外にも大きな山があり、少数test・build・終了不明の処理・hostのほかの負荷を区別する追加の観測が必要。runtimeの自動更新のe2eもworkerの区間には入れていない。
- Rustは同じでも、前後には多数のtestの改善の着地、providerの違い、欠測、終了不明の区間がある。(a)の標本平均17.18→13.80と30以上83→8も、1030や並列度6だけの効果とは切り離せない。並列度6自体の判断はtask 1028の担当で、このtaskはその文書を変更しない。
- 1031・1032を選び直す際は、例外として必要な検証失敗の再現、最後の全体e2e、意図したstressの負荷を分け、終了不明の区間と(c)の山も先に確認する。AGENTS.mdのstressの「負荷が下がるのを待たない」はこの計測では変更しない。
