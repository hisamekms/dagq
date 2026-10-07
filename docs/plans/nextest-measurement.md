---
id: plan-nextest-measurement
type: plan
title: cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest
status: completed
created: 2026-09-26
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - adr-0076
  - adr-0049
  - adr-0078
---

# cargo llvm-cov nextestへの切り替え前後のintegrateのverifyの所要時間と遅いtest

[ADR-0076](../adr/0076-run-the-coverage-gate-tests-with-nextest.md)決定6の前後の測定（task 537）。goal 36（並列数を上げて得をできるようにする）の材料で、遅いtestを削るtaskはplannerがこの文書とgoal 36のnoteを見てtestごとに登録する。この文書はtaskを登録しない。

## 要点

- **nextestで流れたrunは10件に満たない。** 884b3d5の後に着地したruntimeのrunは14件あるが、`cargo llvm-cov nextest`で流れたのはtask 550と551の2件だけ（境界のtask 518を入れて3件）。残りの12件は切り替え前に登録されたtaskで、旧コマンド`cargo llvm-cov`のまま流れた（ADR-0076決定4）。そのため着地時刻での前後の比較は旧コマンド同士の比較になる。旧コマンドと新コマンドの比較はn=2〜3の暫定の値で、10件そろった後に測り直す（task 537のaskの回答A）。
- **期間での前後**: integrateのverify（`land_phases.verify`）の中央値は前302秒・後300秒で変わらない。llvm-covの段は前272秒・後286.5秒。後の期間のrunの大半が旧コマンドなので、差は切り替えの効果ではない。
- **コマンドでの比較（暫定）**: nextestのtest段（`Summary`）は`NEXTEST_TEST_THREADS=4`で200秒・170秒。同じ期間の旧コマンドのtest段（binaryごとの`finished in`の合計）の中央値は231秒。ただしnextestはtest段の後の時間（testの一覧・profrawのmerge・report）が23秒・109秒で、旧コマンドの約20秒より長い。llvm-covの段の全体は266秒・301秒で、旧コマンドの中央値286.5秒とほぼ同じ。ADR-0076が見込んだ「1着地あたり100〜150秒減」はまだ出ていない。
- **律速は並列度で割った合計**。test段の時間は、testごとの所要時間の合計を`NEXTEST_TEST_THREADS`で割った値とほぼ一致する（550: 198.9秒対200.2秒、551: 169.1秒対169.8秒）。最長のtestは21〜24秒で、それより十分長い。testのprocessはほぼ待っているだけで、4本走っていてもCPUは合計0.1〜0.4コアだった。並列度を上げればtest段は縮む見込みで、既定の8で流れた518のtest段は112秒だった。
- **遅いtestの上位10件はtest時間の合計の15%程度**。合計の76%は2秒以上のtest（約140件）が占め、その多くは2〜5秒のruntimeの統合test。上位を1件ずつ削るより、多くのruntimeのtestに共通する待ち（fixture、supervisorの起動、pollの間隔、timeoutを待つ設定）を削るほうが効く。SLOW（60秒超）のtestは無い。
- **測り直し（6章、task 563）**: 並列度8と待ちの短縮（task 566・567）の後、nextestの69 runでllvm-covの段の中央値は208秒（4の期間のnextestは301秒）、test段は163秒、test段の後は6秒になった。SLOWのtestが1件現れ、hostのloadとcmuxの時間切れは増えた。

## 1. 期間の境界

| 項目 | 値 |
| --- | --- |
| 境界のcommit | `884b3d5`（task 518、run `472512db`）。main上のcommit時刻は2026-09-26 15:50:04 JST、`run_integrated`はevent 11911（06:50:05Z） |
| 前の期間 | llvm-covの段の開始が2026-09-26 02:49:24Z（11:49 JST）から06:50:05Zまで。開始はtask 427（`[run.env]`の`CARGO_BUILD_JOBS`・`RUST_TEST_THREADS`）の着地（event 10408）で、`dagq stats --since 10408 --until 11911 --full`の範囲にあたる |
| 後の期間 | 884b3d5より後にllvm-covの段が終わったrun。最後はtask 439のrun `518d0612`（event 13249、10:13:57Z）で、`dagq stats --since 11911 --until 13249 --full`の範囲にあたる。測定時の`next_cursor`は13261 |
| `--parallel` | 両期間とも3。run eventsの`parallel`は2026-09-25 19:47Z（event 8625）までが4、23:34Z（event 9000）以降は3で、前の期間の開始より前に3になっている。後の期間は`supervisor_started`の`parallel`と`claim_parallel`も3 |
| 対象 | 期間内に着地したrunのうち、verificationにllvm-covを含むもの（最後のintegrateの試行）。docsだけのrunは含めない |

前の期間の開始をtask 427の着地に置いたのは、比較の前提を崩す変更を前の期間から外すためである。

- hostのRust toolchainは2026-09-26の11:19〜11:48 JSTの間にRosettaのx86からmiseのarm64に変わった。
- sccache（task 393、ADR-0049決定6）は02:46Zに有効になった。
- `CARGO_BUILD_JOBS=4`・`RUST_TEST_THREADS=4`（task 427）は02:49Zに入った。

`--parallel`が3になったのはそれより前なので、前の期間はどの点でも後の期間とそろっている。

### 比較の前提を崩す重なり

- **後の期間のrunの大半は旧コマンド**: 14件のうち12件は`cargo llvm-cov --locked --fail-under-lines 80`で流れた（ADR-0076決定4により、登録済みtaskのverificationは書き換えない）。nextestで流れたのは550（09:25Z）と551（09:58Z）だけ。
- **境界の518は並列度8**: 518は`dagq.toml`に`NEXTEST_TEST_THREADS = "4"`を足したtask自身で、`integrate`はmain checkoutの`dagq.toml`を読むので、518のverifyはnextestの既定（8並列）で流れた。testごとの合計882.7秒を8で割ると110.3秒で、Summaryの111.6秒と一致する。
- **task 528（06:30Z着地）**: workerが手元で全部の`cargo test`を流さなくなった。後の期間のhostのloadが低いのはこれも一因で、前後のloadの差をnextestの効果とは読めない。
- **task 550（09:30Z着地、8ee936b）**: dev profileのdebug情報を減らした。551以降のbuildとprofrawの大きさに効く。
- **task 551（10:03Z着地、f2d128b）**: ADR-0078でintegration testを1つのbinary `it`にまとめた。551自身のverifyは5 binary（550は47 binary）。551以降の旧コマンドのrun（439）も1 binaryで流れる。
- **検証コマンドごとの内訳**: task 509の`verification_command` eventを使った。llvm-covの段の時間は、同じ試行の直前のコマンド（clippy）のeventとllvm-covのeventの時刻の差で出した。`land_phases.verify`は各runの全試行を含むので、2試行のrunでは段の時間より長い。
- **sccacheの数字**: task 460（`docs/plans/sccache-measurement.md`）は未着地なので参照していない。

## 2. 前後の着地run数・所要時間・load

loadは`~/.local/share/dagq-hostmetrics/metrics.csv`（約30秒ごとの`load1`）の、各runのllvm-covの段の間の平均と最大である。後の期間ではstatsの`load.verify.mean`も取れ、csvの値とほぼ一致した。

| 区分 | run数 | llvm-covの段 中央値（範囲） | `land_phases.verify` 中央値（範囲） | build | test段 | test段の後 | load1 平均の中央値（範囲）／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 前（旧コマンド） | 15 | 272秒（214〜474） | 302秒（219〜726） | 39秒（24〜125） | 220秒（177〜318） | 18秒（5〜31） | 10.1（5.1〜23.6）／29.5 |
| 後（全体） | 14 | 286.5秒（236〜355） | 300秒（242〜372） | — | — | — | 6.1（2.8〜10.4）／14.7 |
| 後のうち旧コマンド | 12 | 286.5秒（236〜355） | 300秒（242〜372） | 36.5秒（25〜68） | 231秒（207〜278） | 20.5秒（3〜35） | 6.1（2.9〜10.4）／14.7 |
| 後のうちnextest（550・551） | 2 | 266秒・301秒 | 284秒・313秒 | 43秒・23秒 | 200秒・170秒 | 23秒・109秒 | 6.5・2.8／8.9 |
| 境界の518（nextest、8並列） | 1 | 206秒 | 213秒 | 31秒 | 112秒 | 63秒 | 7.2／12.5 |

各列の意味は次のとおり。

- **build**: logの最後の``Finished `test` profile ... in``の値。
- **test段**: 旧コマンドではbinaryごとの`finished in`の合計、nextestでは`Summary [ … s]`の値。
- **test段の後**: 段の時間からbuildとtest段を引いた残り。testの一覧（nextestはbinaryごとに`--list`を実行する）、profrawのmerge、reportの時間が入る。logに時刻が無いので、これ以上は分けられない。

runごとの値は次のとおり（時刻はllvm-covの段の開始、JST）。

| 期間 | task | run | 開始 | コマンド | 段 | build | test段 | 後 | `land_phases.verify` | load1 平均／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 前 | 345 | 54a52f8b | 11:58 | 旧 | 231 | 28 | 196 | 8 | 232 | 12.3／16.3 |
| 前 | 403 | abda1589 | 12:19 | 旧 | 214 | 32 | 177 | 5 | 219 | 7.9／10.3 |
| 前 | 335 | a7554f40 | 12:33 | 旧 | 320 | 39 | 261 | 20 | 329 | 13.0／18.0 |
| 前 | 394 | 96a9f439 | 12:46 | 旧 | 264 | 24 | 224 | 15 | 271 | 10.0／15.8 |
| 前 | 310 | 5575d5aa | 13:16 | 旧 | 259 | 35 | 208 | 15 | 428 | 9.8／14.6 |
| 前 | 358 | ecc79d4f | 13:21 | 旧 | 276 | 40 | 220 | 15 | 302 | 10.1／13.5 |
| 前 | 466 | 9498021b | 13:49 | 旧 | 474 | 125 | 318 | 31 | 501 | 23.6／29.5 |
| 前 | 510 | 40aa57cd | 13:58 | 旧 | 342 | 71 | 249 | 22 | 726 | 17.2／22.0 |
| 前 | 490 | de8c2c19 | 14:15 | 旧 | 248 | 26 | 206 | 16 | 255 | 6.1／6.9 |
| 前 | 461 | ebe5c2dd | 14:26 | 旧 | 306 | 64 | 223 | 19 | 462 | 10.8／18.5 |
| 前 | 357 | ac231167 | 14:31 | 旧 | 336 | 67 | 251 | 18 | 352 | 12.5／16.8 |
| 前 | 491 | efcff4e9 | 14:44 | 旧 | 265 | 38 | 211 | 15 | 276 | 6.4／10.4 |
| 前 | 382 | 799c6d99 | 14:49 | 旧 | 265 | 27 | 217 | 20 | 277 | 5.4／8.8 |
| 前 | 492 | b013fe1b | 15:02 | 旧 | 272 | 39 | 215 | 18 | 280 | 5.1／7.5 |
| 前 | 462 | c321a245 | 15:12 | 旧 | 353 | 66 | 258 | 29 | 366 | 10.3／16.6 |
| 境界 | 518 | 472512db | 15:46 | nextest（8並列） | 206 | 31 | 112 | 63 | 213 | 7.2／12.5 |
| 後 | 196 | 902f96b2 | 15:55 | 旧 | 286 | 46 | 219 | 21 | 296 | 6.0／8.8 |
| 後 | 197 | 498446c4 | 16:21 | 旧 | 285 | 32 | 231 | 22 | 292 | 6.7／9.8 |
| 後 | 385 | b6d92d34 | 16:33 | 旧 | 297 | 35 | 242 | 20 | 304 | 6.4／7.8 |
| 後 | 386 | 8d29f958 | 17:06 | 旧 | 286 | 35 | 229 | 22 | 294 | 4.8／5.8 |
| 後 | 429 | 1b3e865b | 17:11 | 旧 | 271 | 31 | 223 | 17 | 283 | 2.9／4.4 |
| 後 | 241 | dbafdbda | 17:36 | 旧 | 355 | 42 | 278 | 35 | 364 | 10.4／14.2 |
| 後 | 514 | 489ef2d7 | 17:44 | 旧 | 287 | 45 | 224 | 18 | 296 | 4.2／7.0 |
| 後 | 445 | 38532947 | 17:57 | 旧 | 300 | 38 | 242 | 20 | 308 | 5.5／9.5 |
| 後 | 325 | 5ec4622a | 18:09 | 旧 | 283 | 31 | 231 | 21 | 372 | 3.7／5.5 |
| 後 | 495 | 5ebae268 | 18:20 | 旧 | 319 | 54 | 243 | 22 | 336 | 6.1／12.2 |
| 後 | 550 | b2f7b36f | 18:25 | nextest | 266 | 43 | 200 | 23 | 284 | 6.5／8.9 |
| 後 | 362 | 1cf47dd2 | 18:47 | 旧 | 328 | 68 | 240 | 20 | 366 | 8.8／14.7 |
| 後 | 551 | 92ffcda6 | 18:58 | nextest | 301 | 23 | 170 | 109 | 313 | 2.8／3.5 |
| 後 | 439 | 518d0612 | 19:10 | 旧 | 236 | 25 | 207 | 3 | 242 | 6.4／9.9 |

読み方:

- 旧コマンドのtest段は、前の期間（loadの中央値10.1）でも後の期間（6.1）でも中央値220〜231秒でほぼ同じ。loadが半分になってもtest段は縮んでいないので、test段はCPUの取り合いではなく、testの中の待ちで決まっている。
- nextestの2件はtest段が31〜61秒短い（200秒・170秒対231秒）。ただし551は「test段の後」が109秒で、段の全体は301秒になり、旧コマンドより長い。551はintegration testを1つのbinary（`it`、387件）にまとめた最初のrun（全体は5 binaryで739件）で、`it`のtestのprocessごとのprofrawが大きなbinary 1本分の計数器を持つので、mergeが重くなった可能性がある（未確認。この時間は分けて測れていない。follow-up）。550（47 binary）の後の時間は23秒で旧コマンド並み、518（46 binary）は63秒。
- `land_phases.verify`はfmtとclippyを含み、2試行のrunではその分も加わる（例: 510の726秒）。

## 3. 遅いtestの上位10件（後の期間のnextestの出力）

`integrate-1-verify-3.log`（llvm-covの段）のnextestの`PASS [ … s]`行から取った。SLOW（`.config/nextest.toml`の`slow-timeout`の60秒超）の行は、550・551・518のどのlogにも無い。最長は21〜24秒。

| 順 | test（551の名前） | 551（5 binary、`it`は1本） | 550（47 binary） |
| --- | --- | --- | --- |
| 1 | `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` | 20.8秒 | 24.0秒 |
| 2 | `runtime_resume::a_request_lost_twice_is_asked_to_the_inbox` | 16.5秒 | 17.2秒 |
| 3 | `runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch` | 11.3秒 | 12.1秒 |
| 4 | `cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` | 10.2秒 | 11.3秒 |
| 5 | `runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step` | 9.3秒 | 12.1秒 |
| 6 | `runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready` | 8.2秒 | 8.9秒 |
| 7 | `runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied` | 7.7秒 | 7.9秒 |
| 8 | `runtime_resume::a_resumed_session_that_ignores_exit_is_let_go` | 6.0秒 | 6.8秒 |
| 9 | `runtime_adopt::independent_tasks_run_concurrently_and_a_dependent_starts_after_integration` | 5.8秒 | 6.1秒 |
| 10 | `runtime_resume::a_lost_request_is_sent_again_after_no_sign_of_work` | 5.5秒 | 6.0秒 |

testの時間の分布は次のとおり（551、739件、合計676.5秒）。

| 所要時間 | 件数 | 合計 | 合計に占める割合 |
| --- | --- | --- | --- |
| 10秒以上 | 4 | 59秒 | 9% |
| 5秒以上 | 17 | 138秒 | 20% |
| 2秒以上 | 136 | 516秒 | 76% |
| 1秒以上 | 195 | 604秒 | 89% |

上位10件の合計は101.5秒で、全体の15%。test moduleごとの合計の上位は`runtime_resume` 136秒、`runtime_review` 81秒、`runtime_integrate` 68秒、`runtime_session` 38秒、`runtime_adopt` 35秒、`cli_version` 34秒、`runtime_triage` 29秒、`runtime_claim` 28秒、`runtime_stall` 28秒。

## 4. 律速の見立てとNEXTEST_TEST_THREADSの余地

| run | 並列度 | testごとの合計 | 合計／並列度 | Summary | 最長のtest |
| --- | --- | --- | --- | --- | --- |
| 518 | 8（既定） | 882.7秒 | 110.3秒 | 111.6秒 | 24.6秒 |
| 550 | 4 | 795.8秒 | 198.9秒 | 200.2秒 | 24.0秒 |
| 551 | 4 | 676.5秒 | 169.1秒 | 169.8秒 | 20.8秒 |

- **律速は「合計÷並列度」**で、最長のtestではない。3本ともSummaryが合計÷並列度と1%以内で一致し、最長のtestの4.5〜8倍ある。nextestは空いたslotに次のtestをすぐ入れるので、今のtestの構成では並列度に比例してtest段が縮む。最長のtest（約21〜24秒）が律速になるのは、並列度がおよそ30を超えてからである。
- **CPUはほぼ空いている**: 551のtest段（他のrunがほぼ動いていない時間帯、`runs`=1）では、testのprocess 4本のCPUは合計11〜39%（0.1〜0.4コア）、`cpu_idle`は61〜75%、load1は2.5〜3.5だった。testのほとんどはstubのsessionやsupervisorをpollで待っている。518（8並列、他のrunのbuildと重なっていた）ではload1が7〜12.5。ただし`test_cpu`はtestのprocessだけを数えており、testが起こす`dagq`・`git`・shのstubの子プロセスは含まない可能性がある。
- **余地**: `NEXTEST_TEST_THREADS`を8にすれば、551の構成でtest段は約170秒から約85秒になる見込み（518の実測は112秒で、そのときの合計は今より大きかった）。1着地あたり約85秒減る。上げたときの懸念は次の2つ。
  1. worker 3本のbuildとintegrateのtestが重なる時間帯にloadが上がり、cmuxのcaptureの時間切れ（goal 36の発端）が増えるおそれがある。
  2. 時間の上限を持つtest（`within`、各testのtimeout）が、待ちの重なりで不安定になるおそれがある。

  上げるなら、まず6か8に上げるtaskにして、`backend_call_failed`の件数と、integrateの検証の失敗（`verification_failed`のresume）を前後で比べるのがよい。この文書では値を変えない。
- **並列度と並べて、test段の後を見る**: nextestにしてもllvm-covの段の全体が縮まない主な理由は、「test段の後」が長くなったこと（518で63秒、551で109秒）である。並列度を上げて縮めた分を打ち消しうるので、先にこの時間の中身（testの一覧・profrawのmerge・report）を測る価値がある。

## 5. 遅いtestごとの短縮の候補

律速が合計÷並列度なので、効くのは「多くのtestに共通する待ち」を削ることで、上位の1件だけを削ってもtest段は（その秒数÷並列度）しか縮まない。候補は次のとおり。どれもコードを読んだ範囲の見立てで、実測していない。

共通の手（2秒以上のruntimeのtest（551で136件）の多くに効く）:

- **supervisorのtickとpollの間隔**: `tests/it/runtime_support`の`TEST_TICK`（50ms）と`idle_poll`、stubのshが`sleep 0.05`で待つloop、helperの`thread::sleep(20ms)`。1つのtestでsupervisorが工程を何周も回すので、tickを短くするか、event（fileの変化）で起こすようにすると、全testが一律に縮む。ただし短くするとCPUとloadは上がる。
- **fixtureの作り方**: testごとの`git init`・seedのcommit・queueのDBの作成・stubの配置。共通のtemplateのrepository（とmigrate済みのDB）を1回作ってcopyするようにすれば、processを分けたnextestでも効く。
- **supervisorの起動と終了の待ち**: testごとに`dagq supervise`を子プロセスで起こし、終わりを待っている。起動の完了を待つpollの間隔と、終了（drain）の待ちを見直す。

testごとの手:

1. `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back`（21〜24秒）: 「docsの変更ではjobが起きない」ことを`std::thread::sleep(3s)`で待ってから否定している（`tests/it/cli_version.rs`の`auto_update`のtest）。否定の待ちを、supervisorが後続のcommitを処理したという観測（event）に置き換える。stubのbuildとsupervisorの入れ替え（execし直し）を3回待つので、handoffのpollの間隔（`--handoff-timeout`と200msのsleep）も候補。
2. `runtime_resume::a_request_lost_twice_is_asked_to_the_inbox`（16〜17秒）: `resume_timeout = 4s`・`start_wait = 1s`・`exit_timeout = 1s`の時間切れを2周待つ設計。時間切れを待つこと自体がtestの中身なので、timeoutの値を下限まで下げる（例: 4秒→1秒）のが一番効く。
3. `runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch`（11〜12秒）: resumeを上限まで繰り返す。同じくresumeの各timeoutとtickを下げる。
4. `cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back`（10〜11秒）: 実際に`install`でsupervisorを引き継がせる。handoffの待ちのpoll（100msのsleep）と`--handoff-timeout`を見直す。
5. `runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step`（9〜12秒）: reviewのjobの失敗を待つ。reviewのstubの失敗までの時間と、sessionを閉じるまでの`exit_timeout`を下げる。
6. `runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready`（8〜9秒）、8. `runtime_resume::a_resumed_session_that_ignores_exit_is_let_go`（6〜7秒）、10. `runtime_resume::a_lost_request_is_sent_again_after_no_sign_of_work`（5.5〜6秒）: どれもresumeのsessionの時間切れ（`resume_timeout`・`exit_timeout`・「作業の兆しが無い」の閾値）を待つ。`runtime_resume`は合計136秒で最大のmoduleなので、moduleの共通のbackendの設定のtimeoutを下げる1本のtaskにまとめられる可能性がある。
7. `runtime_stall::a_session_idle_after_its_nudge_gets_one_stalled_ask_and_its_answers_are_applied`（約8秒）: nudgeからstalledと判定するまでのidleの閾値を待つ。test用の閾値を下げる。
9. `runtime_adopt::independent_tasks_run_concurrently_and_a_dependent_starts_after_integration`（約6秒）: 複数のrunを実際に着地（integrate）させるので、stubの検証コマンドとgitの処理の時間が積み上がる。依存のtaskの開始の観測に必要な最小の構成に絞る。

## 6. 測り直し（並列度8のnextestの69 run）

task 563が、task 566（`NEXTEST_TEST_THREADS`と`RUST_TEST_THREADS`を4→8）とtask 567（統合testに共通する待ちの短縮）の着地の後に、1〜2章と同じ方法で測り直した。対象は10 runを超えてそろったので暫定の値ではない。1〜5章の数字は書き換えていない。4の期間の値は[nextest-test-threads](nextest-test-threads.md)（task 565）から、旧コマンドの値は1〜2章から引用し、測り直していない。

### 要点

- **llvm-covの段は約93秒縮んだ**。中央値は4の期間のnextest（565）の301秒、旧コマンド（2章の「後のうち旧コマンド」）の286.5秒に対し、8の期間は208秒（145〜393）。`land_phases.verify`の中央値は300秒（旧コマンド）・296秒（565）から224秒になった。testの数が737〜754件から798〜1081件に増えたうえでの値である。
- **test段は「合計÷8」で決まり、100件あたりでは約36%短い**。`Summary`の中央値は163秒（4の期間は200秒）で、testの数で割ると100件あたり17.3秒（4の期間は23.0〜31.6秒）。
- **test段の後は短い**。1 binary構成（ADR-0078）で中央値6秒（4〜20）。551の109秒は再現しなかった。
- **新しく律速になったtest**: `runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`はふだん5〜15秒で終わるが、55 runのうち8 runでは126〜128秒かかってSLOWになった。そのrunでは最長のtestがtest段を決め、`Summary`は「合計÷8」の1.31〜1.60倍になった。
- **hostの負荷と不安定さは増えた**。llvm-covの段の間のload1の平均の中央値は13.5（4の期間は5.8〜6.5）。cmuxの`backend_call_failed`は35件（うちcaptureが17件で、4の期間は2件・capture 0件）。時間の上限か競合で落ちたintegrateは5件あった。ただし、`backend_call_failed`の多く（35件のうち21件、captureでは17件のうち14件）はllvm-covの段の外で起きている（6.5節）。
- **8と待ちの短縮の効果は合算である**。566〜567の間の群と566・567の手元の前後測定を見ると、縮んだ分のほとんどは並列度8から来ていて、567の効果は本番では見えるほど大きくない（6.5節）。

### 6.1 対象runと期間

| 項目 | 値 |
| --- | --- |
| 8になった時点 | task 566（6cdf238）の着地は`run_integrated`がevent 14144（2026-09-26 11:11:58Z）。`dagq marks`の`run_env_changed`「`[run.env]` changed: NEXTEST_TEST_THREADS, RUST_TEST_THREADS」はevent 14148（11:11:59Z） |
| 待ちの短縮 | task 567（fe73ada）の`run_integrated`はevent 15046（13:23:44Z、22:23 JST） |
| 対象の期間 | event 15046の後からevent 21344（task 375の着地、2026-09-27 02:05:05Z、11:05 JST）まで。21344は、この測定の保留に使った依存のtask 12件のうち最後に着地したtaskである。範囲は`dagq stats --since 15046 --until 21344 --full`（run 100件、うちllvm-covを流したのが69件） |
| 対象run | 期間内に着地したrunのうち、最後のintegrateの試行が`cargo llvm-cov nextest --locked --fail-under-lines 80`で流れたもの。69件で、全部がnextestだった（旧コマンドのrunは期間内に無い） |
| `--parallel` | 3。期間内の`supervisor_started`はすべて`parallel 3`で、`derived:parallel`の印は無い |
| `NEXTEST_TEST_THREADS`／`RUST_TEST_THREADS` | 8／8（`CARGO_BUILD_JOBS`は4）。期間内に`run_env_changed`は無い |
| toolchain | 1.93.0 aarch64-apple-darwin。次の`derived:toolchain`（1.98.1への変更）は2026-09-27 05:45Zで、期間の後 |
| 別に扱う群 | 566の着地から567の着地までに着地したnextestのrun7件（430・327・238・463・545・424・540）。並列度は8だが、567の待ちの短縮を含まない。対象には混ぜず、6.5節に分けて書く。567自身のrun（baa47f76）は、自分の変更を含む境界のrunなのでどちらにも入れない |

方法は1〜2章と同じ。段の時間は`verification_command` eventの`duration_secs`（565の496では、1〜2章の方法の時刻の差の307秒と一致する）。buildはlogの最後の``Finished `test` profile ... in``、test段は`Summary [ … s]`、test段の後は段からbuildとSummaryを引いた残り。loadは`metrics.csv`の`load1`の、段の間の平均と最大（eventの`load_avg_mean`もほぼ同じ値で、中央値は13.9）。`land_phases.verify`はstatsのrunごとの値。

runごとの値（時刻はllvm-covの段の開始、JST。「試行」はintegrateの試行の数で、値は最後の試行のもの）:

| 着地のevent | task | run | 開始（JST） | 試行 | 段 | build | Summary | test段の後 | `land_phases.verify` | test数 | 最長のtest | load1 平均／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 15117 | 400 | 15de5697 | 09-26 22:24 | 1 | 166 | 40 | 121 | 5 | 181 | 798 | 24 | 12.3／18.9 |
| 15260 | 401 | 6d916365 | 09-26 22:36 | 1 | 224 | 46 | 174 | 5 | 231 | 800 | 30 | 14.8／25.1 |
| 15409 | 425 | b832bb51 | 09-26 22:46 | 1 | 178 | 45 | 129 | 5 | 193 | 804 | 26 | 10.1／11.1 |
| 15536 | 426 | 04dfe261 | 09-26 23:01 | 1 | 215 | 57 | 149 | 8 | 246 | 805 | 29 | 20.0／27.1 |
| 15650 | 573 | 2e5b00b5 | 09-26 23:09 | 2 | 176 | 34 | 137 | 5 | 378 | 809 | 27 | 13.2／17.9 |
| 15697 | 199 | c557c5ed | 09-26 23:12 | 1 | 209 | 39 | 163 | 7 | 227 | 816 | 29 | 23.6／34.2 |
| 15892 | 541 | 3687be84 | 09-26 23:28 | 1 | 252 | 50 | 196 | 6 | 261 | 817 | 28 | 27.2／39.8 |
| 15976 | 470 | 5b1ef3a9 | 09-26 23:37 | 1 | 168 | 38 | 126 | 5 | 177 | 818 | 25 | 11.1／14.6 |
| 16022 | 569 | 595854b9 | 09-26 23:40 | 1 | 243 | 70 | 168 | 5 | 264 | 818 | 28 | 23.6／31.4 |
| 16073 | 593 | a94e1bb2 | 09-26 23:45 | 1 | 168 | 36 | 127 | 4 | 175 | 818 | 23 | 14.1／15.5 |
| 16108 | 543 | 67b90e79 | 09-26 23:51 | 1 | 196 | 31 | 152 | 12 | 209 | 819 | 24 | 16.4／27.5 |
| 16213 | 606 | abdb7db9 | 09-27 00:03 | 1 | 145 | 26 | 114 | 5 | 157 | 820 | 23 | 6.3／9.1 |
| 16480 | 579 | bfd79180 | 09-27 00:18 | 1 | 145 | 27 | 112 | 5 | 152 | 827 | 24 | 6.8／9.9 |
| 16518 | 610 | 3eb3f682 | 09-27 00:20 | 1 | 198 | 55 | 137 | 6 | 216 | 828 | 26 | 17.4／22.9 |
| 16584 | 605 | a0b74709 | 09-27 00:30 | 1 | 174 | 30 | 139 | 5 | 186 | 830 | 25 | 12.2／23.2 |
| 16693 | 497 | a0df50e3 | 09-27 00:40 | 1 | 155 | 27 | 123 | 5 | 162 | 834 | 23 | 9.5／14.1 |
| 16740 | 575 | ebfff19c | 09-27 00:43 | 1 | 176 | 32 | 140 | 4 | 188 | 840 | 25 | 20.6／32.4 |
| 16751 | 498 | f89337ab | 09-27 00:46 | 1 | 148 | 30 | 113 | 5 | 165 | 841 | 24 | 9.9／12.1 |
| 16843 | 591 | a6cbbc75 | 09-27 00:58 | 1 | 153 | 27 | 118 | 7 | 160 | 844 | 25 | 6.9／10.6 |
| 16985 | 377 | cae39c40 | 09-27 01:09 | 1 | 238 | 49 | 181 | 7 | 251 | 852 | 26 | 22.8／46.6 |
| 17054 | 467 | e54f8e04 | 09-27 01:19 | 1 | 186 | 35 | 143 | 9 | 202 | 863 | 30 | 14.8／20.5 |
| 17092 | 242 | a0bcd5e9 | 09-27 01:22 | 1 | 178 | 35 | 133 | 9 | 214 | 863 | 24 | 18.5／28.1 |
| 17136 | 396 | 948e7ddc | 09-27 01:30 | 1 | 163 | 34 | 125 | 5 | 171 | 864 | 25 | 8.5／13.7 |
| 17190 | 559 | 87ff26ce | 09-27 01:36 | 1 | 170 | 30 | 135 | 5 | 178 | 865 | 23 | 14.5／21.2 |
| 17227 | 356 | 9fa9977f | 09-27 01:40 | 1 | 172 | 43 | 125 | 4 | 187 | 867 | 25 | 12.2／13.7 |
| 17300 | 515 | 8a8f522e | 09-27 01:49 | 1 | 152 | 27 | 120 | 5 | 161 | 874 | 23 | 8.6／15.0 |
| 17374 | 359 | d6dcc90b | 09-27 02:05 | 1 | 303 | 58 | 238 | 7 | 343 | 874 | 127 | 18.8／25.6 |
| 17460 | 469 | 813c9340 | 09-27 02:21 | 1 | 161 | 28 | 128 | 5 | 168 | 883 | 25 | 9.0／13.3 |
| 17520 | 599 | 81885a4d | 09-27 02:24 | 1 | 158 | 32 | 122 | 4 | 175 | 884 | 23 | 9.1／10.9 |
| 17545 | 405 | 282aa3b2 | 09-27 02:27 | 1 | 262 | 31 | 227 | 4 | 281 | 888 | 126 | 9.4／15.9 |
| 17628 | 294 | 2a541aea | 09-27 02:40 | 1 | 176 | 33 | 136 | 7 | 190 | 889 | 27 | 10.9／19.0 |
| 17714 | 402 | 8a91ab88 | 09-27 02:50 | 1 | 181 | 34 | 142 | 5 | 199 | 891 | 26 | 12.6／17.1 |
| 17749 | 406 | 1e7aa1bf | 09-27 02:53 | 1 | 166 | 31 | 129 | 6 | 175 | 892 | 23 | 10.2／12.7 |
| 17936 | 295 | 84eb44b8 | 09-27 03:16 | 2 | 258 | 32 | 208 | 18 | 333 | 898 | 26 | 27.3／45.1 |
| 18078 | 387 | 45ed4489 | 09-27 03:29 | 1 | 181 | 37 | 139 | 5 | 197 | 906 | 24 | 10.0／13.3 |
| 18122 | 408 | dbb1b008 | 09-27 03:33 | 1 | 164 | 31 | 129 | 4 | 177 | 907 | 23 | 9.5／12.7 |
| 18282 | 319 | 425e5704 | 09-27 03:51 | 1 | 179 | 46 | 129 | 4 | 188 | 910 | 23 | 10.0／12.8 |
| 18302 | 431 | 8af54e2e | 09-27 03:54 | 1 | 195 | 42 | 144 | 9 | 211 | 925 | 25 | 11.9／14.7 |
| 18371 | 522 | 189448eb | 09-27 04:11 | 1 | 370 | 54 | 312 | 5 | 385 | 927 | 128 | 19.9／30.8 |
| 18395 | 570 | 2ea99280 | 09-27 04:17 | 1 | 184 | 33 | 144 | 7 | 194 | 931 | 26 | 11.2／15.1 |
| 18498 | 571 | 565675bb | 09-27 04:33 | 1 | 199 | 30 | 161 | 8 | 207 | 933 | 24 | 15.5／28.8 |
| 18582 | 619 | a71e73a8 | 09-27 04:41 | 1 | 216 | 37 | 171 | 8 | 225 | 941 | 25 | 15.1／29.1 |
| 18671 | 409 | b7768e75 | 09-27 04:50 | 1 | 312 | 31 | 274 | 7 | 327 | 952 | 128 | 16.9／29.9 |
| 18765 | 621 | f2f7d7ae | 09-27 05:11 | 2 | 287 | 30 | 251 | 7 | 391 | 958 | 127 | 11.1／17.5 |
| 18844 | 620 | a5da06dc | 09-27 05:20 | 1 | 192 | 30 | 158 | 5 | 193 | 962 | 35 | 10.5／14.1 |
| 18936 | 432 | 4c10e12a | 09-27 05:45 | 2 | 253 | 46 | 198 | 9 | 1010 | 976 | 34 | 23.3／29.6 |
| 19006 | 314 | 8a1b2198 | 09-27 06:00 | 1 | 209 | 31 | 173 | 5 | 223 | 976 | 34 | 16.3／24.7 |
| 19031 | 221 | 6fa147a0 | 09-27 06:03 | 1 | 208 | 36 | 167 | 6 | 217 | 989 | 36 | 15.4／20.0 |
| 19237 | 474 | 4ff154ca | 09-27 06:27 | 1 | 329 | 38 | 285 | 6 | 339 | 1003 | 128 | 19.3／32.6 |
| 19359 | 499 | 28182208 | 09-27 06:35 | 1 | 266 | 35 | 211 | 20 | 283 | 1004 | 36 | 23.7／40.5 |
| 19413 | 611 | 340f887f | 09-27 06:44 | 1 | 255 | 36 | 212 | 6 | 275 | 1006 | 37 | 23.2／35.1 |
| 19628 | 397 | 1d0265bf | 09-27 06:57 | 1 | 248 | 53 | 188 | 7 | 262 | 1007 | 37 | 20.1／27.8 |
| 19787 | 520 | 9381a499 | 09-27 07:06 | 1 | 245 | 34 | 205 | 6 | 255 | 1007 | 36 | 22.2／37.8 |
| 20011 | 418 | 3888fd0e | 09-27 08:26 | 2 | 265 | 49 | 208 | 8 | 383 | 1010 | 37 | 23.5／38.0 |
| 20183 | 433 | aefa7b15 | 09-27 08:44 | 2 | 207 | 29 | 173 | 5 | 368 | 1017 | 33 | 12.0／17.2 |
| 20262 | 624 | 911ac534 | 09-27 08:57 | 2 | 209 | 26 | 177 | 6 | 266 | 1019 | 36 | 12.2／21.1 |
| 20299 | 442 | 09f14674 | 09-27 09:01 | 1 | 255 | 60 | 189 | 6 | 275 | 1033 | 36 | 14.6／17.9 |
| 20425 | 420 | 32e5b271 | 09-27 09:16 | 1 | 198 | 30 | 163 | 5 | 206 | 1034 | 35 | 8.3／13.3 |
| 20545 | 577 | 416aeaac | 09-27 09:30 | 1 | 216 | 30 | 180 | 6 | 224 | 1035 | 35 | 11.1／18.1 |
| 20628 | 625 | 845b6559 | 09-27 09:36 | 1 | 292 | 32 | 255 | 5 | 310 | 1036 | 127 | 10.3／16.4 |
| 20852 | 475 | 44aba3e8 | 09-27 09:54 | 1 | 245 | 40 | 199 | 6 | 255 | 1044 | 40 | 12.4／16.2 |
| 20879 | 632 | e9ef74b5 | 09-27 09:58 | 1 | 229 | 50 | 172 | 6 | 255 | 1048 | 36 | 12.1／13.8 |
| 20896 | 240 | 8002e7bc | 09-27 10:02 | 1 | 227 | 39 | 182 | 6 | 247 | 1050 | 33 | 13.5／19.7 |
| 20999 | 250 | d536f89a | 09-27 10:17 | 1 | 278 | 42 | 230 | 6 | 288 | 1052 | 48 | 16.8／24.4 |
| 21057 | 476 | 963383cf | 09-27 10:23 | 2 | 321 | 32 | 283 | 6 | 480 | 1057 | 127 | 14.5／21.2 |
| 21133 | 349 | 07f89a1d | 09-27 10:30 | 1 | 237 | 42 | 187 | 8 | 269 | 1059 | 36 | 12.9／21.3 |
| 21233 | 694 | 1a63b0f0 | 09-27 10:45 | 1 | 245 | 35 | 204 | 7 | 254 | 1062 | 36 | 14.2／23.7 |
| 21317 | 546 | b37041f4 | 09-27 10:53 | 1 | 393 | 137 | 249 | 7 | 416 | 1066 | 43 | 21.2／31.3 |
| 21344 | 375 | 5c2db7e4 | 09-27 11:00 | 2 | 273 | 53 | 208 | 13 | 434 | 1081 | 36 | 18.8／26.2 |

### 6.2 所要時間とloadの比較

| 区分 | run数 | llvm-covの段 中央値（範囲） | `land_phases.verify` 中央値（範囲） | build | test段 | test段の後 | test数 | test段 100件あたり | load1 段の平均の中央値（範囲）／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 旧コマンド（2章の「後のうち旧コマンド」） | 12 | 286.5秒（236〜355） | 300秒（242〜372） | 36.5秒（25〜68） | 231秒（207〜278） | 20.5秒（3〜35） | — | — | 6.1（2.9〜10.4）／14.7 |
| 4の期間の旧コマンド（565の2章） | 14 | 286秒（236〜355） | 296秒（242〜372）※ | 36.5秒（25〜68） | 230秒（201〜278） | — | — | — | 5.8（2.9〜10.4）／14.7 |
| 4の期間のnextest（565の2章） | 3 | 301秒（266〜307） | 同上※ | 43秒（23〜62） | 200秒（170〜238） | 23・109・7秒 | 737〜754 | 23.0〜31.6秒 | 6.5（2.8〜13.0）／15.7 |
| 566〜567の間（8、待ちの短縮なし） | 7 | 176秒（168〜346） | 191秒（175〜655） | 35秒（30〜108） | 139秒（129〜223） | 8秒（4〜16） | 774〜796 | 17.5秒（16.7〜28.7） | 15.3（9.6〜23.8）／35.1 |
| **8の期間（対象）** | 69 | **208秒（145〜393）** | **224秒（152〜1010）** | 35秒（26〜137） | **163秒（112〜312）** | **6秒（4〜20）** | 798〜1081（中央値906） | **17.3秒（13.5〜33.7）** | **13.5（6.3〜27.3）／46.6** |
| 8の期間の最初の10件 | 10 | 194秒（166〜252） | 229秒（175〜378） | 42秒（34〜70） | 143秒（121〜196） | 5秒（4〜8） | 798〜818 | 17.7秒（15.2〜24.0） | 14.4／39.8 |
| 8の期間でSLOWが出なかったrun | 61 | 198秒（145〜393） | 214秒（152〜1010） | 35秒 | 152秒（112〜249） | 6秒 | 798〜1081 | 16.7秒 | 13.2／46.6 |
| 8の期間でSLOWが出たrun | 8 | 308秒（262〜370） | 341秒（281〜480） | 32秒 | 264秒（227〜312） | 6秒 | 874〜1057 | — | 15.7／32.6 |

※ 565の`land_phases.verify`は、llvm-covを流した17 run（旧コマンドとnextestを合わせたもの）の値。「100件あたり」は、565の2章の`Summary`とtest数から計算した（550: 27.2、551: 23.0、496: 31.6）。

読み方:

- **段が縮んだのはtest段とtest段の後による**。buildは35〜43秒で大きく変わらない。test段の中央値は200秒から163秒に、test段の後は4の期間のnextestの7〜109秒から6秒前後になった。test数が増えたので、100件あたりで比べるとtest段は約36%短い。
- **testごとの時間は長くなった**。testごとの時間の合計をtest数で割ると、4の期間の0.92〜1.26秒に対し、8の期間は1.31秒（中央値）だった。loadが高いことに加え、1件12〜15秒の新しいtest（`runtime_waiting_stages`の3件など）が加わった。8で割ったのに半分にならないのはこのためである。
- **期間の中でtestは増え続けた**（798件→1081件）。testごとの時間の合計は955秒から1601秒に増え、test段は期間の前半（最初の10件は143秒）より後半のほうが長い。並列度の比較には「100件あたり」を使う。
- `land_phases.verify`の最大の1010秒はtask 432の2試行（1回目で600秒の時間切れ。6.4節）。

### 6.3 test段の後（1 binary構成）

- task 564（test段の後の中身の分解）は測定の時点で未着地なので、分解は引用できない。
- 8の期間の「test段の後」（段−build−Summary）は中央値6秒（4〜20）。全runが1 binary構成（`it`）で、`Starting … across 5 binaries`だった。
- 2章の551の109秒と、565の550（23秒）・496（7秒）のばらつきは続いていない。69件のうち10秒を超えたのは4件だけで、最大は20秒だった。並列度で縮めた分を「test段の後」が打ち消すという懸念（4章・565の6章）は、この期間には当たらない。

### 6.4 遅いtestの上位10件とSLOW、律速の見立て

69 runの`PASS [ … s]`行から取った、testごとの所要時間の中央値の上位10件（括弧は範囲と出たrun数、右端の列は3章の551／550の値）:

| 順 | test | 中央値（範囲、run数） | 3章（551／550） |
| --- | --- | --- | --- |
| 1 | `cli_version::auto_update_installs_each_runtime_landing_and_puts_a_broken_build_back` | 26.8秒（22.4〜47.8、69） | 20.8／24.0秒 |
| 2 | `runtime_resume::a_request_lost_twice_is_asked_to_the_inbox` | 23.0秒（17.4〜30.8、69） | 16.5／17.2秒 |
| 3 | `runtime_resume::a_resumed_session_gets_its_request_only_once_its_input_box_is_ready` | 17.8秒（9.2〜21.5、69） | 8.2／8.9秒 |
| 4 | `runtime_stall::an_input_the_supervisor_did_not_send_holds_the_nudge_and_is_preempted` | 14.7秒（14.0〜19.3、27） | 無し（新しいtest） |
| 5 | `runtime_waiting_stages::a_question_while_resuming_waits_outside_the_slot_and_gets_its_answer` | 14.6秒（12.6〜19.8、65） | 無し（新しいtest） |
| 6 | `runtime_waiting_stages::a_resume_question_past_the_limit_waits_in_its_slot_with_its_clock_stopped` | 14.2秒（12.0〜19.9、65） | 無し（新しいtest） |
| 7 | `cli_version::install_hands_a_running_supervisor_over_under_its_pid_and_rolls_back` | 12.9秒（9.3〜24.6、69） | 10.2／11.3秒 |
| 8 | `runtime_resume::conflict_only_resumes_are_not_counted_and_a_used_up_run_is_retried_with_its_branch` | 12.5秒（10.0〜17.1、69） | 11.3／12.1秒 |
| 9 | `runtime_waiting_stages::a_question_while_revising_waits_outside_the_slot_with_its_clock_stopped` | 12.4秒（11.2〜17.0、65） | 無し（新しいtest） |
| 10 | `runtime_review::a_failed_review_closes_the_session_and_asks_a_person_in_the_same_step` | 12.1秒（8.5〜22.1、69） | 9.3／12.1秒 |

- 3章からあるtestは、どれも8の期間のほうが長い（loadの差と、8本が同時に走ることによる待ちの伸び）。上位の半分近くは、ADR-0071の待ちのtest（`runtime_waiting_stages`）と`runtime_stall`の新しいtestに入れ替わった。
- **SLOW**: `runtime_stale_receipt::a_stale_receipt_left_during_a_wait_is_unchanged`（task 605がevent 16584で足した）だけが、69 runのうち8 runでSLOW（`> 60s`と`> 120s`）になった。所要時間は126.2〜128.5秒で、そのほかの47 runでは5.2〜15.1秒（中央値7.3秒）だった。はっきり二峰に分かれているので、loadによるばらつきではなく、ある分岐で120秒前後の待ちを踏んでいると見られる（未確認）。期間の後の直近のintegrateのlog 31件でも、14件で100秒を超えている。ほかのtestでSLOWになったものは無い。
- 分布（期間の最後のrun、375、1081件、合計1600.9秒）: 10秒以上が14件・225秒（14%）、5秒以上が99件・784秒（49%）、2秒以上が282件・1416秒（88%）、1秒以上が366件・1532秒（96%）。上位10件の合計は182秒（11%）。test moduleごとの合計の上位は`runtime_review` 210秒、`runtime_resume` 205秒、`runtime_integrate` 132秒、`runtime_triage` 79秒、`runtime_session` 73秒、`runtime_stall_recovery` 72秒、`runtime_waiting_stages` 58秒。5章の見立て（共通の待ちを削るほうが効く）は変わらない。
- **律速の見立て**: SLOWが出なかった61 runでは、`Summary`は「testごとの合計÷8」の1.01〜1.06倍で、537のとき（1%以内）と同じく並列度で割った合計が律速である。SLOWが出た8 runでは1.31〜1.60倍（例: 359は合計÷8が153秒、Summaryが238秒）で、127秒のtestが遅く始まったぶん最長のtestがtest段を決めた。4章では「最長のtestが律速になるのは並列度がおよそ30を超えてから」と見立てたが、このtestがあると並列度8でも律速になる。このtestの120秒前後の待ちを削れば、そのrunのtest段は約110秒縮む（SLOWが出たrunと出なかったrunのSummaryの中央値は264秒と152秒）。

### 6.5 比較の前提を崩す重なり

- **8と待ちの短縮（567）の効果は合算である**。8の期間の値は、並列度8と567の待ちの短縮の両方を含み、本番の数字だけでは分けられない。分かる範囲で切り分けると次のとおり。
  - 566〜567の間の群（8、待ちの短縮なし、7件）: test段100件あたりの中央値は17.5秒、testごとの合計÷test数は1.38秒。8の期間の最初の10件（17.7秒・1.37秒）とほぼ同じで、loadも近い（段の平均の中央値15.3と14.4）。本番では、567の効果は見える大きさでは出ていない。
  - 566のreceiptの手元の前後測定（同じworktree、他のrunと並行）: `cargo llvm-cov nextest`のtest段は、4の188.6秒・184.8秒に対し、8で114.1秒・106.0秒。段の全体は213〜225秒から137〜145秒になった。
  - 567のreceiptの手元の前後測定: baseと変更後を3回ずつ交互に流すと、nextestの`Summary`は121.6・179.0・125.1秒から114.2・110.5・115.3秒になった（loadの近い回で約6〜10%）。ただし`cargo test --test it`とllvm-covのtest段は、縮んだとは示せていない。CPUは約1割増えた。
  - 以上から、4→8で100件あたり23〜32秒から17秒前後に縮んだ分の大半は並列度8によるもので、567の寄与は小さいと見る。567でCPUが増えた分は、8の期間のloadに含まれている。
- **testの数と中身が期間の中で変わった**: 798件から1081件に増えた（+35%）。新しいtestには、ADR-0071の待ちのtest（1件12〜15秒）や、SLOWになるtest（605、event 16584）がある。不安定なtestを直したtaskも期間内に着地した（569: event 16022、593: 16073、520: 19787）。
- **`--parallel`・`[run.env]`・toolchain**: 期間の中では変わっていない（6.1節）。期間の後に、toolchainの1.98.1への変更（2026-09-27 05:45Z）、task 762の不安定なtestの修正（event 23440）、nextestの`retries = 1`（7c8d228、ADR-t768-1）が入ったので、期間は21344で切った。
- **host全体のload**: 期間全体のload1（`metrics.csv`、1430標本）は、平均10.8・中央値8.7・p90 22.4・最大56.9だった。4の期間（565の5章）は5.9・4.9・11.2・20.3。llvm-covの段の間は`cpu_idle`の中央値が4%で、hostはほぼ埋まっている。このときtestのprocessは8本で、CPUは中央値0.69コア・p90 1.14コア（testが起こすgit・dagqの子プロセスは含まない可能性がある）。同じ時間帯にworkerのbuildも走っていた（`rust_n`の中央値6）。`backend_call_failed`の多くはllvm-covの段の外で、主にload 30〜57の時間帯に固まって起きている（04:00〜04:02 JSTに6件、05:39〜05:41に4件、06:18〜06:24に6件）。そのため、loadの増加がすべてtestの並列度から来ているとは言えない。
- **時間の上限と競合で落ちたintegrate**（1回目の試行。どれも2回目で着地）: 時間の上限は3件で、573の`runtime_waiting::a_wrapper_that_goes_silent_during_a_wait_sends_the_run_back_for_its_exit`（30秒の`wait_until`）、433の`runtime_repair::a_recovery_repair_of_a_process_outside_the_run_becomes_an_ask`（30秒）、432の`runtime_repair::a_long_background_alert_is_repaired_by_stopping_the_orphan_of_the_worktree`（600秒の`within`）。eventの読みの競合は2件で、621の`runtime_handoff::auto_update_builds_runtime_landings_and_retries_on_the_answer`（assertの0対1）と、476の`runtime_claim::resident_supervisor_without_runs_is_listed_until_it_stops`（index out of bounds）。どれもそのrunが足したtestではない。ほかの4件は、変更の中身による失敗（295・418・375の`lifecycle_replace::up_applies_compatible_migrations_and_refuses_breaking_ones`。migrationの巻き戻しが足りなかった）と、環境による失敗（624のSQLiteの`disk I/O error`）だった。566〜567の間の群では、327の`runtime_session::unanswered_exit_request_times_out_and_keeps_the_run`（競合。566の手元でも4と8の両方で出ていた。238（event 14573）と567が、到達した状態を待つ形に直した）が1件あった。

566〜567の間の群のrunごとの値:

| 着地のevent | task | run | 開始（JST） | 試行 | 段 | build | Summary | test段の後 | `land_phases.verify` | test数 | 最長のtest | load1 平均／最大 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 14205 | 430 | a4d37125 | 09-26 20:37 | 1 | 168 | 30 | 129 | 8 | 175 | 774 | 29 | 9.6／11.9 |
| 14447 | 327 | 0b24da09 | 09-26 21:16 | 2 | 346 | 108 | 223 | 15 | 655 | 779 | 38 | 22.7／29.9 |
| 14573 | 238 | 451a04e3 | 09-26 21:36 | 1 | 327 | 90 | 221 | 16 | 359 | 782 | 34 | 23.8／35.1 |
| 14720 | 463 | cc75b1df | 09-26 21:55 | 1 | 310 | 88 | 213 | 9 | 503 | 791 | 55 | 19.6／28.0 |
| 14976 | 545 | 2788c02c | 09-26 22:11 | 1 | 172 | 35 | 133 | 4 | 180 | 793 | 32 | 10.9／14.9 |
| 14999 | 424 | 24893fb7 | 09-26 22:14 | 1 | 176 | 31 | 139 | 6 | 189 | 795 | 23 | 15.3／28.1 |
| 15012 | 540 | 49af8a50 | 09-26 22:17 | 1 | 172 | 32 | 136 | 4 | 191 | 796 | 25 | 13.3／14.7 |

327・238・463はloadが高い時間帯（段の平均19.6〜23.8）に流れ、buildが88〜108秒と長いので、段も310〜346秒と長い。

### 6.6 並列度の判断

8のままにするか、6か4に戻すかの見立ては、4の期間と同じ指標で並べて[nextest-test-threads](nextest-test-threads.md)の7章に書いた。この文書は値を変えない。
