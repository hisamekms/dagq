---
id: plan-nextest-test-threads
type: plan
title: NEXTEST_TEST_THREADSとRUST_TEST_THREADSが4の期間の基準値と、8への変更後の比べ方
status: active
created: 2026-09-27
updated: 2026-09-29
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-nextest-measurement
  - adr-0076
  - adr-0049
---

# NEXTEST_TEST_THREADSとRUST_TEST_THREADSが4の期間の基準値と、8への変更後の比べ方

task 566（2026-09-26に人がplannerと決めた）が`dagq.toml`の`[run.env]`の`RUST_TEST_THREADS`と`NEXTEST_TEST_THREADS`を4から8に上げた。その前後を比べるため、4の期間の基準値をこの文書に残す（task 565、goal 36のacceptance (4)）。変更後の測定は着地後の10 run以上がそろってから行い、この文書の6章の方法で比べて、8のままか6か4に戻すかを決める。この文書は値を変えない。

## 要点

- **期間**: 両方が4だったのは、task 518の着地（event 11911、2026-09-26 06:50:05Z）からtask 566の着地（`[run.env]`の変更の印 event 14148、11:11:59Z）まで。`--parallel`は3。
- **llvm-covの段**（integrateの`cargo llvm-cov`／`cargo llvm-cov nextest`のコマンド1本）: 成功した17試行で中央値286秒（236〜355）。うちnextestは3件だけ（266・301・307秒）で、残り14件は旧コマンド（`RUST_TEST_THREADS=4`がbinaryの中のthread数に効く）。
- **test段**: nextestの`Summary`は200・170・238秒（中央値200秒）。旧コマンドのbinaryごとの`finished in`の合計は中央値230秒（201〜278）。
- **`backend_call_failed`**: 2件（どちらもcmuxの`exists`の`backend_timeout`で、retryで回復。exhaustedは0）。captureの時間切れは0件。
- **integrateの検証の失敗によるresume（`verification_failed`）**: 1件（task 325）。llvm-covを流したrun 17件の5.9%。原因はmigrationの内容（`duplicate column name`）で、並列度や時間の上限とは関係ない。時間の上限を持つtestの失敗は0件。
- **load1**（`metrics.csv`、約30秒ごと）: 期間全体で平均5.9・中央値4.9・p90 11.2・最大20.3。llvm-covの段ごとの平均の中央値は5.8（旧コマンド）。

## 1. 期間の境界

| 項目 | 値 |
| --- | --- |
| 始まり | task 518（884b3d5、`NEXTEST_TEST_THREADS = "4"`を足した）の`run_integrated`、event 11911（2026-09-26 06:50:05Z、15:50 JST）。`RUST_TEST_THREADS=4`はそれより前のtask 427の着地（event 10408、02:49Z）から効いている |
| 終わり | task 566（6cdf238、2026-09-26 20:11:58 JST）の着地。`dagq marks`の`run_env_changed`「`[run.env]` changed: NEXTEST_TEST_THREADS, RUST_TEST_THREADS」がevent 14148（11:11:59Z）。566自身のverifyはfmtだけなので、どちらの期間の数字にも入らない |
| statsの範囲 | `dagq stats --since 11911 --until 14148 --full`（run 20件、うちllvm-covを流したrun 17件） |
| `--parallel` | 3（期間内の`supervisor_started`はすべて`parallel 3`） |
| 対象 | 期間内に`verification_command`のeventがあるintegrateの試行（`phase: integration`）。docsだけのrun（494・537・566）はllvm-covを流さないので段の値に入らない |

注意:

- event 12608（08:49:58Z）の`run_env_changed`は、`[run.env]`の記録を始めたbuildの最初の記録で、値の変更ではない。
- 518自身のverifyはnextestの既定（8並列）で流れたので、期間に入れていない（[nextest-measurement](nextest-measurement.md)の1章）。
- 期間の前半（06:50〜09:30Z）は[nextest-measurement](nextest-measurement.md)の「後の期間」と重なる。値は同じ方法で取り直した。
- 期間の中で比較の前提が変わった点: task 550（09:30Z、dev profileのdebug情報を減らした）、task 551（10:03Z、ADR-0078でintegration testを1つのbinary `it`にまとめた）。551より後の旧コマンドのrun（439・441・468）はtest段が201〜207秒で、前の旧コマンドのrun（中央値231秒）より短い。

## 2. integrateのllvm-covの段とtest段の所要時間

方法は[nextest-measurement](nextest-measurement.md)の2章と同じ。段の時間は同じ試行の直前のコマンド（clippy）の`verification_command` eventとllvm-covのeventの時刻の差、buildはlogの最後の``Finished `test` profile ... in``、test段はnextestでは`Summary [ … s]`、旧コマンドではbinaryごとの`finished in`の合計。loadは`~/.local/share/dagq-hostmetrics/metrics.csv`の`load1`の、段の間の平均と最大。

| 区分 | 試行数 | llvm-covの段 中央値（範囲） | build 中央値（範囲） | test段 中央値（範囲） | load1 段の平均の中央値（範囲）／最大 |
| --- | --- | --- | --- | --- | --- |
| 全体（成功した試行） | 17 | 286秒（236〜355） | — | — | — |
| 旧コマンド（`RUST_TEST_THREADS=4`） | 14 | 286秒（236〜355） | 36.5秒（25〜68） | 230秒（201〜278） | 5.8（2.9〜10.4）／14.7 |
| nextest（`NEXTEST_TEST_THREADS=4`） | 3 | 301秒（266〜307） | 43秒（23〜62） | 200秒（170〜238） | 6.5（2.8〜13.0）／15.7 |

`land_phases.verify`（fmt・clippyと全試行を含む）はllvm-covを流した17 runで中央値296秒（242〜372）。

runごとの値（時刻はllvm-covの段の開始、JST）:

| task | run | 開始 | コマンド | 段 | build | test段 | load1 平均／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 196 | 902f96b2 | 15:55 | 旧 | 286 | 46 | 219 | 6.0／8.8 |
| 197 | 498446c4 | 16:21 | 旧 | 285 | 32 | 231 | 6.7／9.8 |
| 385 | b6d92d34 | 16:33 | 旧 | 297 | 35 | 242 | 6.4／7.8 |
| 386 | 8d29f958 | 17:06 | 旧 | 286 | 35 | 229 | 4.8／5.8 |
| 429 | 1b3e865b | 17:11 | 旧 | 271 | 31 | 223 | 2.9／4.4 |
| 325（試行1、失敗） | 5ec4622a | 17:35 | 旧 | 74 | 32 | 37 | 6.2／6.7 |
| 241 | dbafdbda | 17:36 | 旧 | 355 | 42 | 278 | 10.4／14.2 |
| 514 | 489ef2d7 | 17:44 | 旧 | 287 | 45 | 224 | 4.2／7.0 |
| 445 | 38532947 | 17:57 | 旧 | 300 | 38 | 242 | 5.5／9.5 |
| 325（試行2） | 5ec4622a | 18:09 | 旧 | 283 | 31 | 231 | 3.7／5.5 |
| 495 | 5ebae268 | 18:20 | 旧 | 319 | 54 | 243 | 6.1／12.2 |
| 550 | b2f7b36f | 18:25 | nextest | 266 | 43 | 200 | 6.5／8.9 |
| 362 | 1cf47dd2 | 18:47 | 旧 | 328 | 68 | 240 | 8.8／14.7 |
| 551 | 92ffcda6 | 18:58 | nextest | 301 | 23 | 170 | 2.8／3.5 |
| 439 | 518d0612 | 19:10 | 旧 | 236 | 25 | 207 | 6.4／9.9 |
| 441 | ec828dc0 | 19:19 | 旧 | 251 | 35 | 201 | 4.4／5.6 |
| 468 | fe884c5a | 19:34 | 旧 | 247 | 39 | 204 | 4.3／5.4 |
| 496 | 47b0d7f2 | 19:53 | nextest | 307 | 62 | 238 | 13.0／15.7 |

nextestの3件のtestごとの時間（`PASS [ … s]`の合計）と律速:

| run | test数 | testごとの合計 | 合計÷4 | Summary | 最長のtest | test段の後（段−build−Summary） |
| --- | --- | --- | --- | --- | --- | --- |
| 550 | 737 | 795.8秒 | 198.9秒 | 200.2秒 | 24.0秒 | 23秒 |
| 551 | 739 | 676.5秒 | 169.1秒 | 169.8秒 | 20.8秒 | 109秒 |
| 496 | 754 | 949.2秒 | 237.3秒 | 238.1秒 | 26.5秒 | 7秒 |

3件ともSummaryは「合計÷並列度」と1%以内で一致する。8にすればtest段はおよそ半分（合計が同じなら85〜120秒）になる見込み。「test段の後」は7〜109秒とばらつき、並列度で縮めた分を打ち消しうる（task 564が中身を測る）。

参考: `RUST_TEST_THREADS=4`だけが効いていた直前の期間（event 10408〜11911、旧コマンド15件）は、llvm-covの段が中央値272秒（214〜474）、test段が220秒（177〜318）、load1の段の平均の中央値が10.1（[nextest-measurement](nextest-measurement.md)の2章の「前」）。

## 3. backend_call_failed

`dagq events --after 11911 --all --full --kind backend_call_failed`（event 14148まで）と`dagq stats`の`backend_failures`:

| 項目 | 値 |
| --- | --- |
| 件数 | 2（event 12674・12675、08:52Z） |
| op | `exists` 2件（同じworkspaceの1回目と2回目の試行、`backend_timeout`、cmuxの`Command timed out`） |
| retryで回復／使い切り | 2／0（`exhausted` 0） |
| そのときのload_avg | 15.7・20.0（load帯 8-16が1件、16-32が1件）、slots 3 |

## 4. integrateの検証の失敗によるresume

| 項目 | 値 |
| --- | --- |
| `verification_failed`のresume | 1件（task 325、run 5ec4622a の試行1） |
| 割合 | llvm-covを流したrun 17件の5.9%（期間のrun 20件の5%）。integrateのllvm-covの試行18回のうち失敗1回 |
| 原因 | `tests/lifecycle_replace.rs`の`up_applies_compatible_migrations_and_refuses_breaking_ones`が`apply queue migration 37: duplicate column name: answered_by`で落ちた。変更の中身の失敗で、並列度・時間の上限・loadとは関係しない（試行2で着地） |
| 時間の上限を持つtestの失敗 | 0件（`within`の時間切れ・timeoutで落ちたtestは無い。`stats`の`failed_tests.tests`も空） |
| 他のresume | `rebase_conflict` 3件、`sent_back` 1件、`unknown` 1件（並列度とは関係しない） |

## 5. loadとcmuxのcaptureの時間切れ

| 項目 | 値 |
| --- | --- |
| load1（期間全体、`metrics.csv`の496標本、15:50〜20:11 JST） | 平均5.9、中央値4.9、p90 11.2、最大20.3 |
| load1（llvm-covの段の間） | 段ごとの平均の中央値5.8（旧コマンド）、全試行の最大15.7（496） |
| cmuxのcaptureの時間切れ | 0件（`backend_call_failed`の`op: capture`は無い） |
| cmuxの他の時間切れ | `exists` 2件（3章） |

goal 36の発端（並列4、target分離前の設定）では、load averageが最大151〜204、captureの時間切れが400件を超えていた（goal 36のnote 8718）。4の期間はそれより2桁小さい。

## 6. 変更後に比べる方法と戻す目安

### 測り方

566の着地（event 14148）の後、llvm-covを流して着地したrunが10件以上そろったら、1〜5章と同じ指標を同じ方法で取る。

- 範囲: `dagq stats --since 14148 --until <10件目以降の着地のevent> --full`。KPIの前後は`dagq kpi --compare 14148 --kind runtime`でも読む。
- llvm-covの段・build・test段・test段の後: `verification_command` eventの時刻の差と`integrate-<試行>-verify-<N>.log`（2章の方法）。**旧コマンドとnextestを分けて比べる**（ADR-0076決定4で旧コマンドのtaskが残るため。旧コマンドは`RUST_TEST_THREADS`、nextestは`NEXTEST_TEST_THREADS`が効く）。nextestでは「合計÷8」とSummaryの一致も確かめる。
- `backend_call_failed`: opごとの件数とload帯、`exhausted`。
- `verification_failed`のresume: 件数とllvm-covを流したrun数に対する割合、失敗したtestの名前と原因（時間の上限・timeoutによる失敗か）。
- load1とcaptureの時間切れ: `metrics.csv`の期間全体と段の間の平均・中央値・p90・最大、`op: capture`の件数。
- 期間の中で他の変更（task 564など、test段の後やtestの待ちを変えるもの）が着地したら、その境界を書き、前後を分けて読む。

### 判断の目安

8のままにするのは、test段（nextest）の中央値が4の期間（200秒）より縮み、次のどれも悪化していないとき。

次のどれかが起きたら6に戻し、6でも起きるなら4に戻す。

- cmuxのcaptureの時間切れが出る（4の期間は0件）、または`backend_call_failed`がllvm-covの段と重なって増える（4の期間は2件でどれもretryで回復）、または`exhausted`が出る。
- llvm-covの段の間のload1の平均の中央値が4の期間（5.8）の2倍程度（12前後）を超える、または最大が20（4の期間の期間全体の最大）を大きく超える。
- 時間の上限を持つtest（`within`・各testのtimeout）の失敗で`verification_failed`のresumeが出る（4の期間は0件）。変更の中身による失敗（4章のtask 325のような）は数えない。
- test段は縮んだが、llvm-covの段の全体が縮まない（test段の後が伸びて打ち消す）。このときは並列度ではなくtest段の後を先に見る（task 564）。

値を戻すときは、`dagq.toml`の`[run.env]`の2つを同じ値にそろえ、AGENTS.mdの`dagq.toml`の項とこの文書に結果を書く。

## 7. 変更後（8）

task 563が、6章の方法で8の期間の値を取った（565のfollow-upを引き受けた）。対象はtask 567（統合testの共通の待ちの短縮）の着地（event 15046、2026-09-26 13:23:44Z）の後から、event 21344（2026-09-27 02:05:05Z）までに着地したrunのうち、llvm-covを流したもの69件で、全部がnextest（`NEXTEST_TEST_THREADS=8`）だった。範囲は`dagq stats --since 15046 --until 21344 --full`で、`--parallel`は3、期間内に`[run.env]`とtoolchainの変更は無い。runごとの値・遅いtest・比較の前提は[nextest-measurement](nextest-measurement.md)の6章にある。1〜5章の4の期間の値は変えていない。

8の期間の値は、並列度8と567の待ちの短縮の効果を合算したものである。566〜567の間に着地した7件（8、待ちの短縮なし）はtest段100件あたりが17.5秒で、567の後の最初の10件（17.7秒）とほぼ同じだったので、縮んだ分の大半は並列度8から来たと見る（nextest-measurementの6.5節）。

### 7.1 指標の並び

| 指標 | 4の期間（1〜5章） | 8の期間 | 変化 |
| --- | --- | --- | --- |
| llvm-covの段（nextest） | 301秒（266〜307）、3件 | 208秒（145〜393）、69件 | 約93秒短い |
| build | 43秒（23〜62） | 35秒（26〜137） | ほぼ同じ |
| test段（nextestの`Summary`） | 200秒（170〜238） | 163秒（112〜312） | 短い |
| test段 100件あたり | 23.0〜31.6秒（testは737〜754件） | 17.3秒（13.5〜33.7、testは798〜1081件） | 約36%短い |
| `Summary`÷（testごとの合計÷並列度） | 1.00〜1.01 | 1.01〜1.06。SLOWのtestが出た8件は1.31〜1.60 | 最長のtestが律速になるrunが出た |
| test段の後 | 23・109・7秒 | 6秒（4〜20） | 短い |
| `land_phases.verify` | 296秒（242〜372、旧コマンドを含む17 run） | 224秒（152〜1010） | 短い |
| `backend_call_failed` | 2件（`exists` 2、exhausted 0） | 35件（`capture` 17、`exists` 16、`create_named` 1、`listed_workspace_ids` 1。retryで回復28、exhausted 7） | 増えた |
| cmuxのcaptureの時間切れ | 0件 | 17件（exhausted 2）。llvm-covの段と重なったのは3件 | 出た |
| `backend_call_failed`のload帯 | 8-16が1件、16-32が1件 | 0-4が1件、8-16が2件、16-32が11件、32-64が21件（eventの`load_avg`の最大57.6） | 高いloadで起きている |
| `verification_failed`（時間の上限・競合によるもの） | 0件 | 5件（llvm-covを流した69 runの7.2%）。時間の上限3件、eventの読みの競合2件。どれも2回目の試行で着地 | 出た |
| `verification_failed`（変更の中身・環境によるもの） | 1件（325） | 4件（295・418・375のmigrationの巻き戻し、624のSQLiteの`disk I/O error`） | — |
| load1（llvm-covの段の間の平均の中央値） | 5.8（旧コマンド）・6.5（nextest） | 13.5（6.3〜27.3） | 約2.2倍 |
| load1（llvm-covの段の間の最大） | 15.7 | 46.6 | 約3倍 |
| load1（期間全体、`metrics.csv`） | 平均5.9、中央値4.9、p90 11.2、最大20.3 | 平均10.8、中央値8.7、p90 22.4、最大56.9（1430標本） | 約2倍 |

時間の上限と競合で落ちたtestは次のとおり（どれもそのrunが足したtestではない）。

- 時間の上限: `runtime_waiting::a_wrapper_that_goes_silent_during_a_wait_sends_the_run_back_for_its_exit`（task 573、30秒）、`runtime_repair::a_recovery_repair_of_a_process_outside_the_run_becomes_an_ask`（433、30秒）、`runtime_repair::a_long_background_alert_is_repaired_by_stopping_the_orphan_of_the_worktree`（432、600秒）
- 競合: `runtime_handoff::auto_update_builds_runtime_landings_and_retries_on_the_answer`（621）、`runtime_claim::resident_supervisor_without_runs_is_listed_until_it_stops`（476）

`backend_call_failed`の35件のうち、llvm-covの段と時間が重なったのは14件だけだった。残りは段の外の、主にload 30〜57の時間帯に固まって起きている（04:00〜04:02 JST、05:39〜05:41、06:18〜06:24）。llvm-covの段の間はhostがほぼ埋まっていた（`cpu_idle`の中央値4%）。このとき8本のtestのprocessのCPUは中央値0.69コアで、同じ時間帯にworkerのbuildが走っていた（`rust_n`の中央値6）。

段の外の3つの山の出どころは、task 932が[load-spike-2026-09-27](load-spike-2026-09-27.md)で調べた（workerが手元で流した範囲の広いtestの重なり）。

### 7.2 6章の目安に照らした見立て

| 6章の目安 | 8の期間 | 当たるか |
| --- | --- | --- |
| test段（nextest）の中央値が4の期間（200秒）より縮む | 163秒（100件あたり約36%短い） | 縮んだ |
| captureの時間切れが出る、`backend_call_failed`が段と重なって増える、`exhausted`が出る | captureの時間切れ17件、段と重なったもの14件、exhausted 7件 | 当たる |
| 段の間のload1の平均の中央値が12前後を超える、または最大が20を大きく超える | 13.5、最大46.6 | 当たる |
| 時間の上限を持つtestの失敗で`verification_failed`のresumeが出る | 3件（競合を合わせて5件） | 当たる |
| test段は縮んだが段の全体が縮まない | 段も約93秒縮んだ | 当たらない |

**見立て**: 目安を字のとおりに当てはめると「6に戻す」になる。test段と段の全体は縮んでいるが、悪化を示す3つの目安（captureの時間切れ、load、時間の上限による失敗）がそろって当たっている。ただし、次の点から、悪化のすべてが`NEXTEST_TEST_THREADS=8`によるとは言えない。

- `backend_call_failed`の6割とcaptureの時間切れの8割は、llvm-covの段の外で起きている。
- 期間の中でtestは35%増え、567でtestのCPUが約1割増えた。
- 時間の上限と競合による失敗は、1件ずつ見るとeventの読みの競合や30秒の待ちで、loadが上がると露わになる不安定なtestである。期間の後にはtask 762が不安定なtestを直し、nextestの`retries = 1`（ADR-t768-1）で、不安定なtestだけの失敗はworkerに返らなくなった。

そこで、次の順にするのがよいと見る。

1. 両方の値を6に下げる（`dagq.toml`の`[run.env]`の2つをそろえる）。下げたら、7.1節と同じ指標を10 run以上で取る。100件あたりのtest段が8より約1/3伸びる代わりに、段の間のloadとcaptureの時間切れが4の期間の水準（段の間のloadの中央値12未満、capture 0件）に戻るかを見る。戻らなければ、並列度ではなくhostの他の負荷（段の外のloadの山）を先に調べる。
2. 1と並べて、`runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`の約127秒の分岐を直す。このtestは8の期間の8 run（期間の後の直近では31件のうち14件）で最長のtestになってtest段を決め、そのrunの段を約110秒延ばしている（段の中央値は308秒と198秒）（nextest-measurementの6.4節）。

値を戻すtaskはこの文書からは登録しない（receiptのfollow_upsに書く）。

## 8. 6に下げた（task 930）

task 930が`dagq.toml`の`[run.env]`の`RUST_TEST_THREADS`と`NEXTEST_TEST_THREADS`を両方8から6に下げた。`CARGO_BUILD_JOBS`（4）と`--parallel`（3）は変えていない。1〜7章の値は変えていない。

### 8.1 下げた理由

7.2節の見立てのとおり。8の期間はtest段（nextest）の中央値が163秒に縮み（100件あたり約36%短い）、llvm-covの段の全体も約93秒縮んだが、6章の悪化の目安のうち3つがそろって当たった。

- captureの時間切れが17件出て、`backend_call_failed`の`exhausted`が7件あった（4の期間は0件）。
- llvm-covの段の間のload1の平均の中央値が13.5で目安の12前後を超え、最大は46.6だった。
- 時間の上限と競合による`verification_failed`のresumeが69 run中5件あった（4の期間は0件）。

6章の「どれかが起きたら6に戻す」に従って6に下げた。AGENTS.mdの方針（悪化すれば6か4に戻す）の範囲の判断なので、新しいADRは書いていない。悪化のすべてが並列度8によるとは言えない（7.2節）ので、4まで一度に戻さず、6で測ってから決める。

### 8.2 6の期間の測り方

- 境界: task 930の着地。`dagq marks`の`run_env_changed`（`[run.env]` changed: NEXTEST_TEST_THREADS, RUST_TEST_THREADS）のeventを始まりにする。
- llvm-covを流して着地したrunが10件以上そろったら、6章の測り方と同じ指標（llvm-covの段・build・test段・test段100件あたり・test段の後・`backend_call_failed`とload帯と`exhausted`・captureの時間切れ・`verification_failed`の原因別の件数・load1）を取り、7.1節の表に4・8・6の3列で並べる。範囲は`dagq stats --since <境界のevent> --until <10件目以降の着地のevent> --full`、KPIは`dagq kpi --compare <境界の印> --kind runtime`でも読む。
- 期待するのは、test段100件あたりが8より約1/3伸びる代わりに、段の間のload1の平均の中央値が12未満、captureの時間切れが0件に近づくこと。
- 期間の中で他の変更（task 931・933のtestの直し、task 932の調査から出た変更など）が着地したら、その境界を書き、前後を分けて読む。

### 8.3 6でも戻らないとき

6でも段の間のloadとcaptureの時間切れが4の期間の水準に戻らなければ、並列度を4に戻す前に、hostのほかのloadを先に見る。7.1節のとおり`backend_call_failed`の6割とcaptureの時間切れの8割はllvm-covの段の外（04:00〜06:30 JSTのloadの山）で起きているので、その出どころの調査（task 932）の結果を先に読む。並列度が原因と言えるときだけ4に戻す。

また、test段を律速しうる`runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`の約127秒の分岐（7.2節の2、task 931・933）が6の期間にも重なる。このtestが最長になったrunは段が約110秒延びるので、test段と段の全体の比較ではそのrunを分けて読み、931・933の着地の前後も分ける。
