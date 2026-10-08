---
id: plan-cmux-inbox-only
type: plan
title: cmux を inbox だけが使う形（goal 92）の前後の、着地の関門の test 段・e2e・cmux の fake を使う it の数字
status: active
created: 2026-10-09
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - plan-integration-to-unit-tests
  - plan-it-reduction
  - plan-landing-it-selection
---

# cmux を inbox だけが使う形（goal 92）の前後の、着地の関門の test 段・e2e・cmux の fake を使う it の数字

goal 92（対話の worker と runtime の planner の対話の経路、run の workspace、supervisor・queue service・observer の cmux、in-cmux mode を runtime から消し、tests/it と e2e を減らす）の前後を、記録済みの log・event と repository から数えた（task 1444）。関門も e2e も流していない。後の関門の log は 10 件に 1 件足りない 9 件なので、関門の値は暫定（4 章）。

## 要点

- **cmux の fake（tests の WorkspaceBackend の実装）を使う tests/it の #[test] は 765→62 本（直接名指すものは 509→62 本）、ファイルは 105→9。** 後の 62 本は `up` / `down` / `install` の lifecycle の test（lifecycle_up・lifecycle_down・lifecycle_replace・lifecycle_plan と、`up` を呼ぶ installed_plugin・landing_branch・main_checkout・queue_service・source_repository）だけで、goal の acceptance (3) の形になっている。
- **e2e の #[test] は 17→12 本（`#[ignore]` の e2e は 15→10 本と cleanup の unit test 2 本）、1 回に流れる本数は 15→9〜10 本。** task 1443 の着地の後の 6 回（参考。10 件に足りない）は 1 回 58〜114 秒（中央値 69 秒）で、前の 10 回の 103〜298 秒（中央値 236 秒）より短い。cmux が答えない host では `up` の test 1 本だけが飛ばされ、残りの 9 本が流れた。ただし受け入れの選び方の後の標本（task 1436 の着地の後の最初の 10 件、2026-10-03）は 66〜566 秒（中央値 175.5 秒）で、その日の前半の e2e はまだ 14 本だった。
- **関門の test 段は縮んでいない。** test の数は 2,496→3,112（平均）、test の時間の合計は 2,451.0→3,481.3 秒（平均）、Summary の壁時計は中央値 405.3→646.2 秒、land_phases.verify は中央値 587→697 秒。対話に固有の test（キーワードの分類）の時間は 649.7→279.8 秒、cmux の fake を使う test は 1,263.1→23.3 秒に減ったが、それ以外の dagq::it が 385.4→2,934.7 秒（分類のない unknown を含む）に増えた。
- **減らない理由の見立て（5 章）:** (1) 消した test より移した test と同じ期間に足された test が多い（dagq::it の本数は 1,113→1,095 とほぼ同じ、unit は 1,406→2,000 本）。(2) 偽の cmux で即座に終わっていた runtime の test が、非対話の background の経路（wrapper の process・turn の file）を通るようになり、1 本あたりが重くなった（dagq::it の 1 本あたり 2.14→2.96 秒。runtime_cleanup は 1 本あたり 5.4→12.9 秒）。(3) 後の期間の中で、12:00Z 以降の 5 本は lib を含む全 module が一様に約 1.7 倍に伸びていて（早い 4 本の test の時間の合計の平均 2,537.7 秒、遅い 5 本 4,236.2 秒）、host の混み具合が効いている。前は coverage の計測（llvm-cov）つき、後は計測なしの nextest なので、同じ条件なら後の方がさらに長いはず。
- 後の関門の log が 9 件で 10 件に足りないので、再測を receipt の follow_ups（measurement）に出した（4 章）。

## 1. 手順

1. event と stats は queue service を通る読み取りの CLI（queue.read）で読み、`$TMPDIR/cmux-events/` に保存した。`dagq events` は古い順に 100 件ずつ返すので、出力の `cursor` を次の `--after` に渡し、空のページが返るまで送った。

   ```sh
   fetch() {  # 名前 kind since until
     after=0; i=0
     while :; do
       i=$((i+1)); out=$TMPDIR/cmux-events/$1-page$i.json
       dagq events --full --kind $2 --since $3 --until $4 --after $after > $out
       n=$(python3 -c 'import json,sys;print(len(json.load(open(sys.argv[1]))["events"]))' $out)
       after=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["cursor"])' $out)
       [ "$n" = 0 ] && break
     done
   }
   fetch before-integrated run_integrated   2026-10-02T00:00:00Z     2026-10-02T15:25:00Z
   fetch after-integrated  run_integrated   2026-10-08T10:09:15.224Z 2026-10-08T16:56:50.125Z
   fetch before-e2e        run_e2e_finished 2026-10-02T00:00:00Z     2026-10-02T15:25:00Z
   fetch after-e2e         run_e2e_finished 2026-10-03T08:26:24.042Z 2026-10-08T16:56:50.125Z
   dagq stats --since 2026-10-02T00:00:00Z --until 2026-10-02T15:25:00Z --full > $TMPDIR/cmux-events/before-stats.json
   dagq stats --since 2026-10-08T00:00:00Z --until 2026-10-08T16:56:50.125Z --full > $TMPDIR/cmux-events/after-stats.json
   ```

   取ったページ数と件数（空の最後のページを含む）:

   | 名前 | ページ | event |
   | --- | ---: | ---: |
   | before-integrated | 2 | 49 |
   | after-integrated | 2 | 17 |
   | before-e2e | 2 | 18 |
   | after-e2e | 3 | 113 |

   `dagq stats --since <時刻>` は時刻ちょうどに終わった run（task 1443 の run）を含まなかったので、後の stats は 2026-10-08T00:00:00Z からにして run の ID で選んだ。

2. 集計の script は [measure.py](cmux-inbox-only/measure.py)（Python の標準ライブラリだけ）。repository の root で次を打つ。log の dir は既定値の無い必須の引数 `--runs-dir` で受け取り、log・保存した JSON・git の object を読むだけで、queue と runs の下に書かず dagq を呼ばない（`dagq locate`・`--db`・DB の直接の読み取りは使わない）。

   ```sh
   python3 docs/plans/cmux-inbox-only/measure.py \
     --runs-dir ~/.local/share/dagq/77067154921b9014/runs \
     --events-dir "$TMPDIR/cmux-events" --repo . \
     --out-dir docs/plans/cmux-inbox-only
   ```

   出力の CSV（log の path は `--runs-dir` からの相対、列は measure.py が決める）:

   - [gate-logs.csv](cmux-inbox-only/gate-logs.csv): 選んだ関門の log（前 3・後 9）ごとの値
   - [gate-excluded.csv](cmux-inbox-only/gate-excluded.csv): 後の期間に着地して外した run と理由
   - [gate-modules.csv](cmux-inbox-only/gate-modules.csv): dagq::it の module ごとの log あたりの秒と本数（前・後）
   - [e2e-events.csv](cmux-inbox-only/e2e-events.csv): 取った run_e2e_finished の全件（標本の印、log から数えた流れた本数と落ちた本数）
   - [it-tests-before.csv](cmux-inbox-only/it-tests-before.csv)・[it-tests-after.csv](cmux-inbox-only/it-tests-after.csv): tests/it の #[test] ごとの cmux の fake の使用と対話のキーワードの分類

3. 区分と作り直せる範囲: measure.py はこの 1 回の測定の道具、CSV はその出力で、runs の dir の log・queue の event・repository から作り直せる派生の表（正本は元の記録）。script が `--events-dir` で読む event と stats の JSON は commit せず `$TMPDIR` に置いたので、作り直すには 1. のコマンドで取り直す。それができるのは runs の dir の log と queue の event が残っている間だけで、消えた後はこの CSV が残る唯一の記録になる。tests/it と e2e の本数は git の commit から何度でも作り直せる。

## 2. 選び方と期間

### 関門

- **前（基準）:** goal 92 の登録（2026-10-02T15:25Z）の直前に着地し、関門（`cargo llvm-cov nextest`）が最初の試行で全て通った 3 本。log は 3 本とも残っていた。

  | run | task | 着地 | log | test の数 | Summary の秒 |
  | --- | ---: | --- | --- | ---: | ---: |
  | 4ea3a630-3fbb-43e1-8e03-b97274756187 | 1389 | 2026-10-02T14:50:09Z | 4ea3a630-…/integrate-1-verify-3.log | 2,495 | 402.0 |
  | baeccaaa-dbf7-4d3c-b44b-443060b5207b | 648 | 2026-10-02T15:12:29Z | baeccaaa-…/integrate-1-verify-3.log | 2,496 | 405.3 |
  | e6fceb15-7afe-43c8-bf11-e6b8c4fe12d2 | 1329 | 2026-10-02T15:23:27Z | e6fceb15-…/integrate-1-verify-3.log | 2,497 | 445.5 |

- **後:** task 1443 の着地（run 4a04eaf5、2026-10-08T10:09:15.224Z）とそれより後に着地した run を着地の古い順に見て、integrate の最初の試行（`integrate-1-verify-*.log`）の nextest が全て通ったものを最大 10 件。締切はこの task の run の claim（2026-10-08T16:56:50.125Z）。この期間の関門は ADR-t1925-1 の `[landing_verification]` が置き換えた `sh scripts/landing-it.sh`（計測なしの nextest で unit 全件と IT）で、IT を全部流した log（`landing-it: every IT runs`）だけを採った（そのうち 45f67d1f は 1 本、c3e9400e と e1ac7213 は 2 本の planner_headless の IT を `left out, fails on main already` で外している）。期間の 17 件の着地のうち 9 件が当たり、外した 8 件は次のとおり。

  | 理由 | run（task） |
  | --- | --- |
  | 最初の試行で落ちた | fd7065b8（2128）、26972e1a（1554）、dd06164e（1553）、a4d8e39e（1847） |
  | IT を絞った関門（2,045 本） | 312c1f8c（2063） |
  | 関門の test を流さない task | 5ae18eca（1544）、71d151d3（2154）、765595e5（1902） |

  選んだ 9 件: 4a04eaf5-ddd6-4d26-b31d-707395c4fb2c（1443）、633477d9-3699-4bc4-b8cd-403da2d22505（2098）、9d1bceca-a992-4fed-94e3-8b9b51f75f49（1494）、45f67d1f-d4cf-4a59-8742-5f8d1db995ac（1552）、b8b19652-fc18-48d4-94a7-447eeffb03f9（2018）、30f12d7f-9be2-436d-b3de-b7bb287561d6（1556）、c8fbf81d-232d-4e85-bb4a-b7ab2bee8bf6（2155）、c3e9400e-db19-428e-86cd-8f7041f86aab（1650）、e1ac7213-97f4-4290-9153-ca373f3ba9c1（1715）。

- 並列度は前後とも `[run.env]` の `NEXTEST_TEST_THREADS = "6"`（dagq.toml、両方の base の commit）。

### e2e

- 標本は期間の中の run_e2e_finished のうち outcome が passed か failed のもの。時間（secs・lock_wait_secs）は標本だけで、outcome ごとの件数と割合は期間の全件で数える。
- **前:** 2026-10-02T15:25Z より前の直近 10 件。期間は標本の最も古い event（2026-10-02T07:01:52.020Z）から 2026-10-02T15:25Z。
- **後:** task 1436 の着地（2026-10-03T08:26:24.042Z）より後の古い順に 10 件。期間は task 1436 の着地からこの run の claim（2026-10-08T16:56:50.125Z）。
- **参考（後、1443 の後）:** 同じ規則で task 1443 の着地からこの run の claim まで。標本は 6 件で 10 件に足りない。
- 標本の run は [e2e-events.csv](cmux-inbox-only/e2e-events.csv) の `sample`・`sample_after_1443` の印。

### cmux の fake を使う it

- 前は goal 92 の最初の task（1436）の着地の直前の main（151a4955）、後はこの run の base（63e3bfa0）。同じ script で数えた。
- 偽の cmux の型は、tests の下の `impl WorkspaceBackend for <型>` の型（前: TestWorkspace・PlanWorkspace・FakeCmux・NoCmux、後: FakeCmux・NoCmux）。後の TestWorkspace は background の起動の fake で、WorkspaceBackend を実装していない。
- 「使う」は、#[test] の本文がその型を名指すか、その型を本文や型に持つ support の fn・struct（tests/common と tests/it の support の module は全ファイルから、それ以外は同じファイルから）を呼ぶもの。「直接」は型を本文が名指すものだけ。comment の行は見ない。
- 同じ grep での確かめ: `git grep -lE 'FakeCmux|NoCmux|TestWorkspace|PlanWorkspace' <commit> -- tests/it` は型の名前が出るファイルで、後は TestWorkspace が fake でなくなったので数が合わない。本数は script の分類を正とする。

## 3. 数字

### 着地の関門の test 段

前は 3 本、後は 9 本の log の平均・中央値・幅。

| 値 | 取得元 | 前 | 後 |
| --- | --- | --- | --- |
| test の数 | Summary の `N tests run` | 平均 2,496（2,495〜2,497） | 平均 3,112.1（3,085〜3,142） |
| test の時間の合計（秒） | 結果の行の秒の和（Summary の行より前） | 平均 2,451.0・中央値 2,376.1（2,353.2〜2,623.8） | 平均 3,481.3・中央値 3,851.4（2,160.9〜4,806.1） |
| test 段の壁時計（秒） | Summary の秒 | 平均 417.6・中央値 405.3（402.0〜445.5） | 平均 587.8・中央値 646.2（369.5〜806.1） |
| land_phases.verify（秒） | `dagq stats --full` の run の `land_phases.verify` | 平均 587・中央値 587（517〜657） | 平均 792.3・中央値 697（472〜1,284） |
| 検証の工程の load1 の平均 | 同じ run の `load.verify.mean` | 平均 17.8（16.5〜18.6） | 平均 10.8（5.8〜16.1） |
| flaky | 結果の行の `FLKY-FL` | 0 | 0 |

land_phases.verify は integrate の検証の工程全体（fmt・clippy・build・設計文書の検査、前は llvm-cov の report を含む）の時間で、nextest の test 段の壁時計（Summary の秒）とは別の値。

test の時間の合計の内訳（log あたりの平均の秒、括弧は本数）。dagq::it の test は、前は 151a4955、後は 63e3bfa0 の本文で分類した。「対話」は本文が goal 92 の description のキーワード（`screen`・`dialog`・`/exit`・`exits_sent`・`answer_prompt`・`stuck_exit`・`interactive`・`resend`・`enter`、大文字小文字を区別しない）を含むもの、「fake」は対話でなく cmux の fake を使うもの、「unknown」は分類の commit に名前の無い test（log の run の base が分類の commit と違う）。

| 内訳 | 前 | 後 |
| --- | ---: | ---: |
| lib・bin・crates の unit | 138.8（1,406） | 228.5（2,000.4） |
| dagq::it: 対話 | 649.7（228.3） | 279.8（81.8） |
| dagq::it: fake | 1,263.1（487.7） | 23.3（57） |
| dagq::it: その他 | 253.1（299） | 2,897.7（947.3） |
| dagq::it: unknown | 132.3（59） | 37.0（8.6） |
| plugin | 13.9（16） | 14.9（17） |
| dagq::it の計 | 2,298.3（1,074） | 3,237.9（1,094.7） |

後の「対話」の 82 本は、`run screen` や `interactive` の拒否を確かめる test などキーワードを含むだけのもので、対話の経路の test ではない。fake を直接名指す test だけに絞ると、前は対話でないもの約 312 本・約 755 秒、後は 57 本・約 24 秒。

dagq::it の module ごとの差の大きいもの（log あたりの平均の秒、括弧は本数。全部は [gate-modules.csv](cmux-inbox-only/gate-modules.csv)）:

| module | 前 | 後 |
| --- | ---: | ---: |
| runtime_cleanup | 52.0（9.7） | 241.0（18.7） |
| runtime_recheck | 19.6（4） | 101.1（16） |
| planner_slots | 4.0（4） | 51.4（10） |
| 前の log に無い 39 module（review_subagents・recovery_codex・planner_headless_turns・planner_headless・provider_executables など。前の 3 本より後に足された） | — | 計 約 526 |
| plan_review | 31.0（30） | 66.0（15） |
| runtime_integrate | 122.9（35） | 100.0（23） |
| runtime_session | 60.6（30） | 26.1（13） |
| 後の log に無い 20 module（runtime_stall_recovery・runtime_resume_exit_retry・runtime_exit_retry・runtime_resume_adopt・runtime_screen_idle*・runtime_stall・runtime_review_exit・runtime_review_background・runtime_review_adopt など。goal 92 でない runtime_broker 48.1 秒を含む） | 計 約 350 | — |

goal 92 の description の数字との照合: test の数 2,496 は一致。合計 2,278 秒は dagq::it の計（2,298.3 秒）に近く、全 binary の和（2,451.0 秒）ではない。壁時計 約 405 秒は中央値 405.3 秒と一致。対話に固有 約 230 件・約 644 秒は 228.3 件・649.7 秒と近い。偽の cmux を通る 約 400 件・約 853 秒は、この文書の分類では直接名指すもの約 312 件・約 755 秒、support の fn を通るものまで含めると 487.7 件・1,263.1 秒で、description の分類（本文のキーワードの概算）とは数え方が違う。両方を残す。

### e2e

| 値 | 取得元 | 前 | 後（1436 の後） | 参考: 1443 の後 |
| --- | --- | --- | --- | --- |
| #[test] の数 | tests/e2e.rs と tests/e2e/ の `#[test]`（前 151a4955、後 63e3bfa0） | 17（`#[ignore]` 15、cleanup の unit 2） | 12（`#[ignore]` 10、cleanup の unit 2） | 同左 |
| 1 回に流れた本数 | 標本の e2e-<attempt>.log の `test … ok / FAILED` の行 | 15（10 回とも） | 10〜14（中央値 14） | 9〜10（中央値 9） |
| secs（秒） | 標本の payload の secs | 平均 214.1・中央値 236.0（103〜298） | 平均 218.0・中央値 175.5（66〜566） | 平均 74.2・中央値 69.0（58〜114） |
| lock_wait_secs（秒） | 標本の payload の lock_wait_secs | 平均 19.6・中央値 0（0〜196） | 平均 130.7・中央値 0（0〜1,186） | 0（6 回とも） |
| 標本 | | 10 | 10 | 6 |

outcome ごとの件数（期間の全件が分母）:

| 期間 | 分母 | passed | failed | unavailable |
| --- | ---: | --- | --- | --- |
| 前（2026-10-02T07:01:52.020Z〜2026-10-02T15:25Z） | 10 | 10（100%） | 0（0%） | 0（0%） |
| 後（2026-10-03T08:26:24.042Z〜2026-10-08T16:56:50.125Z） | 113 | 111（98.2%） | 0（0%） | 2（1.8%） |
| 参考: 1443 の後（2026-10-08T10:09:15.224Z〜2026-10-08T16:56:50.125Z） | 6 | 6（100%） | 0（0%） | 0（0%） |

前の期間は標本の最も古い event から始まるので、分母は標本の 10 件と同じになり、後の 5 日分の分母（113）とは割合の重みが違う。unavailable の 2 件は 2026-10-04T04:05Z（6357a771、717 秒）と 2026-10-05T07:50Z（379c0da8、1,800 秒）。

後の標本は 2026-10-03 の 10 件で、goal 92 の task のうち着地していたのは 1436 だけだった。その前半 7 件は 14 本が流れた。e6cfc445 の 1 回目は lock を 1,186 秒待った後（secs に待ちは含まない）、14 本のうち 5 本が落ちて落ちた test を流し直し（`e2e-1.rerun.log`）、outcome は passed のまま 566 秒かかった。表の「1 回に流れた本数」は落ちた行も数える。後半 3 件（15:33Z 以降）は 10 本で 66〜81 秒。1443 の後の 6 件で流れたのは 9〜10 本で、cmux が答えない host では `up_starts_a_launchd_supervisor_that_status_lists_and_down_wait_stops_it` だけが飛ばされ（log の冒頭に理由がある）、ほかは流れた（acceptance (5)）。

### cmux の fake を使う it

| 値 | 前（151a4955） | 後（63e3bfa0） |
| --- | ---: | ---: |
| tests/it の #[test] | 1,113 | 1,095 |
| cmux の fake を使う（support の fn を通るものを含む） | 765 | 62 |
| うち型を直接名指す | 509 | 62 |
| 使う test のあるファイル | 105 | 9 |
| 対話のキーワードを含む | 235 | 82 |

後の 62 本のファイル: lifecycle_up 29、lifecycle_replace 14、lifecycle_down 9、installed_plugin 3、landing_branch 2、main_checkout 2、lifecycle_plan 1、queue_service 1、source_repository 1。どれも `up` / `down` を FakeCmux か NoCmux で呼ぶ test。

## 4. 件数が足りない値と再測

- 後の関門の log は 9 件（10 件に 1 件足りない）。締切（claim）までの 6.8 時間に 17 件が着地し、うち 9 件が条件に当たった（約 1.3 件/時）。10 件目は claim から約 1 時間で揃う見込みで、test の数と test の時間の合計は 1 件でも読めるので、件数不足が主に効くのは壁時計と land_phases.verify。
- 後の期間の中で host の混み具合による揺れが大きい（3 章の幅、5 章 (3)）ので、再測は 2026-10-09T12:00Z 以降に、task 1443 の着地以降の同じ条件の log 10 件（揃えば 20 件）で、load と並べて行う。receipt の follow_ups（measurement）に出した。
- e2e の 1443 の後の参考の標本は 6 件。同じ再測で 10 件にする。

## 5. 減らない理由の見立て

- **(1) test は消すより移した。** goal の制約（共通の判断は非対話の経路か unit test に移して残す）どおり、対話と workspace の test の多くは非対話の経路の test か unit test に移った。dagq::it は 1,113→1,095 本でほぼ同じで、unit は 1,406→2,000 本に増えた（goal 68・goal 100 の移し替えを含む）。同じ期間に別の goal が足した module（review_subagents・recovery_codex・planner_headless_turns・runtime_ci_watch など）もある。
- **(2) 1 本あたりが重くなった。** 偽の cmux の test は session の起動も画面も fake で即座に終わったが、移った test は非対話の background の起動（wrapper の process と turn の file）を通る。dagq::it の 1 本あたりは 2.14→2.96 秒、runtime_cleanup は 1 本あたり 5.4→12.9 秒、runtime_recheck は 4.9→6.3 秒。
- **(3) 後の期間の host の混み具合。** 後の 9 本のうち 11:15Z までの 4 本は test の時間の合計の平均 2,537.7 秒・Summary の平均 432.4 秒で、前（2,451.0 秒・417.6 秒）に近い。12:00Z 以降の 5 本は 4,236.2 秒・712.2 秒で、dagq の lib の binary の unit（119.5→206.6 秒。gate-modules.csv に無く、log から module と同じ方法で binary ごとに足した値）を含む全 module が一様に伸びている。test の変更ではなく host の混み具合の差に見えるが、load1 の平均（`load.verify.mean`）はむしろ前より低く、この値だけでは説明できない。
- **(4) 前後で関門の形が違う。** 前は llvm-cov の計測つきの nextest、後は ADR-t1925-1 の landing-it（計測なし）。計測は test を遅くするので、同じ形なら後の値はさらに大きい。比べる値は、計測の有無が効かない test の本数と、分類ごとの時間の割合で読むのがよい。
- e2e は本数（15→9〜10）と 1 回の時間（中央値 236→69 秒）の両方が減っていて、goal の狙いどおり。関門の test 段の短縮は、goal 92 の範囲では (1)・(2) のために出ていない。
