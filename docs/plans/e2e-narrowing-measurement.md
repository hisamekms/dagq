---
id: plan-e2e-narrowing-measurement
type: plan
title: dagq.tomlの[e2e] pathsでworkerのe2eを狭めた前後の、runtimeのrunのe2eの時間とworkの時間
status: completed
created: 2026-10-26
owners:
  - hisamekms
tags:
  - performance
  - testing
  - measurement
related:
  - adr-t963-1
  - adr-t1233-2
  - plan-nextest-test-threads
---

# dagq.tomlの[e2e] pathsでworkerのe2eを狭めた前後の、runtimeのrunのe2eの時間とworkの時間

goal 66のacceptance「導入の前後でruntimeのrunのe2eの時間とworkの時間が比べられている」の測定（task 1026）。task 966が`dagq.toml`に`[e2e] paths`を置き、runtimeのtaskに一律に`--evidence e2e`を付ける規則をやめた。その前と後のruntimeのrunを、固定バイナリ`~/.local/bin/dagq`の読み取り（`stats --full`・`kpi`・`marks`・`events`）だけで比べる。後の対象は42 run（10 run以上）で、暫定の値ではない。

## 要点

- **e2eを流したrunの割合**: 前は91.7%（一律）。後（狭めた直後、workerがe2eを流していた期間）は45.9%（paths 18 runのうちwork_breakdownのある17 runの全部、not_required 24 runは0）。e2eが要るrunの割合は、その後の期間（e2eがhostに移った後）でも41.7%（652 runのうち272）で、goal 66の分析の「src/のcommitの38%が狭い範囲に触れる」とほぼ合う。
- **work に占める e2e の割合**: 前12.2% → 後7.0%。e2eの合計は前の11.5時間（252 run）から後の0.95時間（42 run）。work_breakdownのあるrun 1本あたりに直すと182秒から93秒で、約半分になった。
- **e2eを流したrun 1本あたりのe2eの時間の中央値**: 前125秒・後154秒でほぼ変わらない。減ったのは流すrunの数。
- **workの中央値**: 前1182秒・後1157秒。e2eの減り（流さないrunで約2分）は、workの中央値ではほかの変動に埋もれて小さい。not_requiredの群のworkの中央値は867秒で、pathsの群（1529秒）より短い。
- **e2eの出どころ**: 後の期間全体（966の着地から2026-10-26まで、runtimeのrun 726本）ではnot_required 404・paths 235・task 87。taskは966より前に登録され`--evidence e2e`を持ったままのtaskで、87本のうち48本が最初の1週に着地した。pathsの群を起こしたファイルは`src/infrastructure/adapters.rs`が最も多い（235本のうち91本）。
- **重なり**: 狭めた直後の期間（2026-09-29〜10-01）はClaudeの停止とCodexでの代行、e2eの関門の失敗の続き（`update_failed`のstage e2eが15回）と重なり、runtimeの着地数とfirst passの率が下がった。workの比較にはこの重なりが入る。e2eの時間と割合の比較はrunごとのwork_breakdownから取るので、この重なりの影響は小さい。

## 1. 対象のrunと期間

| 項目 | 値 |
| --- | --- |
| 境界 | task 966の着地、`run_integrated` event 40839（2026-09-28T23:59:58Z）。commit a494154e0 |
| 前 | `dagq stats --since 2026-09-26T00:00:00Z --until 40839 --full`。2026-09-26T00:10Z〜09-28T23:40Z に着地した runtime の run（`areas`に`runtime`を持ち`status`が`integrated`）252本。`runs[].e2e`はこの期間のrunでは`null`がほとんど（一律の`--evidence e2e`の期間で、群に分けられない） |
| 後 | `dagq stats --since 40839 --until 56724 --full`。966の着地から、task 1239の着地（event 56724、2026-10-01T21:03:54Z）までに着地したruntimeのrun 74本。そのうち`runs[].e2e`がpathsかnot_requiredのもの42本（paths 18・not_required 24）を「後」とし、taskの32本は別の群にする |
| 後の終わりの理由 | task 1239（ADR-t1233-2）がe2eをworkerから外し、reviewのpassの後にruntimeがhostで流す工程にした。event 56724より後のrunでは、workerのwork_breakdownにe2eがほぼ出ない（e2eは`land_phases.e2e`に出る）ので、workerのe2eの比較はここで切る。その後の期間は3章で別に添える |
| `--parallel` | 3（前と後の`claim_parallel`は3。前の21本は値が無い） |

前の範囲はgoal 66の分析と同じ「26日以降」。goal 66の記述の「126本で計6時間」はe2eのコマンド1本ずつの数え方で、この文書の値（work_breakdownの`secs.e2e`を252 runで合わせた11.5時間、e2eのコマンド453回）とは数え方が違う。

## 2. 前と後の比較

workerのe2eの時間は`runs[].work_breakdown.secs.e2e`、回数は`work_breakdown.commands.e2e`。「e2eを流したrun」は`secs.e2e`が0より大きいか`commands.e2e`があるrun。割合とe2eの時間はwork_breakdownのあるrunだけで数える（前は228本、後は37本。無いrunはsessionのtranscriptが取れなかったもの）。workに占める割合は、work_breakdownのあるrunのe2eの合計をそのrunのworkの合計で割ったもの。

| 群 | run | work_breakdownあり | e2eを流したrun | e2eの時間の中央値（流したrun） | e2eの合計 | workの中央値 | workの合計 | workに占めるe2e | `land_phases.verify`の中央値 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 前（一律の`--evidence e2e`） | 252 | 228 | 209（91.7%） | 125秒 | 41,428秒（11.5時間） | 1182秒 | 105.4時間 | 12.2% | 350.5秒 |
| 後（paths＋not_required） | 42 | 37 | 17（45.9%） | 154秒 | 3,431秒（0.95時間） | 1157秒 | 14.8時間 | 7.0% | 537秒 |
| 後のうちpaths | 18 | 17 | 17（100%） | 154秒 | 3,431秒 | 1529秒 | 8.7時間 | 11.7% | 520秒 |
| 後のうちnot_required | 24 | 20 | 0（0%） | — | 0秒 | 867秒 | 6.1時間 | 0% | 577.5秒 |
| 別の群: task（`--evidence e2e`を持ったまま） | 32 | 30 | 30（100%） | 187.5秒 | 8,203秒（2.3時間） | 1164.5秒 | 17.0時間 | 13.4% | 520.5秒 |

- 1 runあたりのe2eの時間（work_breakdownのあるrunで割ったもの）は、前が182秒（41,428 / 228）、後が93秒（3,431 / 37）。
- e2eのコマンドの失敗は、前が453回のうち1回、後のpathsが33回のうち1回、taskの群が70回のうち6回。
- `land_phases.verify`（integrateの検証）が前350秒から後537秒に延びたのは、e2eとは関係しない。integrateはe2eを流さない（ADR-t963-1決定4）。4章の`[run.env]`の変更とtestの増加が重なっている。
- workの時間の中に入っていても`chain`（`&&`でつないだコマンド）に分類されたe2eは`secs.e2e`に入らない。前の期間はwork_breakdownのある228本のうち219本が`chain`を持ち、e2eの時間は少なめに出ている可能性がある。

参考: `dagq kpi --area runtime --compare 40839 --window 3`（前後3日）の`phase.work`のarea=runtimeの中央値は1182秒 → 1122秒（verdict improved）、`land_phase.verify`は350秒 → 541秒、`landings`は252 → 84、`first_pass_rate`は0.73 → 0.50。後の3日は event 56724 より後の約1日を含む。

## 3. 後の群のe2eの出どころとpathsの傾向

966の着地から2026-10-26T02:31Zまでの`dagq stats --since 40839 --full`の、着地したruntimeのrun 726本の`runs[].e2e`の内訳。

| `runs[].e2e` | run | 最初の週（09-29〜10-05） | 2週目 | 3週目 | 4週目 |
| --- | --- | --- | --- | --- | --- |
| not_required | 404 | 118 | 101 | 60 | 125 |
| paths | 235 | 82 | 71 | 41 | 41 |
| task | 87 | 48 | 8 | 8 | 23 |

event 56724より後の652本では、e2eはhostの工程（`land_phases.e2e`）で流れた。

| 群（event 56724より後） | run | hostでe2eを流したrun | hostのe2eの中央値 | hostのe2eの合計 | workの中央値 |
| --- | --- | --- | --- | --- | --- |
| paths | 217 | 217 | 143秒 | 49,072秒（13.6時間） | 2005.5秒 |
| not_required | 380 | 0 | — | 0秒 | 1396秒 |
| task | 55 | 55 | 137秒 | 10,636秒（3.0時間） | 1937秒 |
| 計 | 652 | 272（41.7%） | 143秒 | 59,708秒（16.6時間） | 1588.5秒 |

hostのe2eの合計はこの652本のworkの合計の4.4%で、e2eの待ち（`land_phases.e2e_wait`）は合わせて0.9時間。workerのwork_breakdownにe2eが残ったのはこの期間の16本（3,852秒）。

pathsの群のrunが触れたe2eの範囲のファイル（着地のcommitの差分を、各時点の`[e2e] paths`のglobに照らしたもの。235本）:

| ファイル | run |
| --- | --- |
| `src/infrastructure/adapters.rs` | 91 |
| `src/application/integrate.rs` | 56 |
| `src/infrastructure/codex.rs` | 37 |
| `src/application/headless_session.rs` | 36 |
| `src/application/actor_executor.rs` | 33 |
| `src/infrastructure/sessions.rs` | 32 |
| `src/application/lifecycle.rs` | 29 |
| `tests/e2e.rs`・`tests/e2e/**` | 22・21 |
| `src/application/update.rs` | 21 |
| `src/infrastructure/e2e_gate.rs` | 20 |
| ほか（session・install・handoff・update・process・launchd・binaries・claudeなど） | 各17以下 |

- 235本のうち128本は1つのファイルだけでe2eが要るとされた。`adapters.rs`は範囲の中で最も多くのrunを起こしている（39%）。
- 着地のcommitの差分で数えたので、validatingの時点の差分とはずれがある。not_requiredのrunのうち2本は着地のcommitでは範囲に触れていた（範囲に後から足されたglobか、validatingより後の変更）。

## 4. 比較の前提を崩す重なり

| 重なり | 前 | 後（event 40839〜56724） |
| --- | --- | --- |
| `--parallel` | 3 | 3 |
| `[run.env]`（`dagq marks`の`run_env_changed`） | 2026-09-26 08:49Z（CARGO_BUILD_JOBS・NEXTEST_TEST_THREADS・RUSTC_WRAPPER・RUST_TEST_THREADS・SCCACHE_IGNORE_SERVER_IO_ERROR）、11:11Z（NEXTEST_TEST_THREADS・RUST_TEST_THREADS） | 966の約3.5時間前の2026-09-28 20:26Z にNEXTEST_TEST_THREADS・RUST_TEST_THREADSが変わり、後の期間の全体に効いている |
| Claude Code | 2.1.283 | 2.1.283が主。09-29に2.1.284と2.1.283を往復し、10-01 14:59Zから2.1.286 |
| provider・host・workerの手元の検証（`dagq marks`の`mark_recorded`） | 09-28 09:33Z workerの手元のstressを20周から5周に縮めた（ADR-t920-1）。前の期間の終わりの約14時間だけに効く | 09-29 17:52Z goal reviewをCodexに。09-30 05:14Z Claudeを止め、Codexのworkerと臨時のCodexのinboxで代行（後の74本のうち`actual_provider`がcodexのrunは7本で、work_breakdownは無い）。09-30 10:41Z・10:48Z Codexの既定のmodelの変更。10-01 14:59Z codex 0.155.1 → 0.159.2 |
| toolchain | 09-27 05:45Z 1.93.0 → 1.98.1 | 1.98.1のまま |
| e2eの関門（task 964）が入れ替えを止めた回数（`update_failed`のstage e2e） | 1回（966の着地の前、event 39103） | 15回（09-29 01:21Z〜10-01 13:55Z、14のbuild）。後の期間（09-29〜10-01 21:04Z）の`update_installed`は55回 |

- 後の期間は着地数が少なく（kpiの前後3日でruntimeの着地252 → 84）、first passの率も下がった。Claudeの停止と関門の失敗の続きが主な理由と見られ、e2eを狭めたことの効果ではない。workの中央値の比較はこの重なりを含む。
- e2eの時間と割合はrunごとのwork_breakdownから取るので、着地数の違いは割合の比較には効かない。
- event 56724より後（3章）は、e2eのhostへの移動（ADR-t1233-2）、run reviewのCodexへの移動（10-02）、Claudeのworkerのheadless化（10-02）、着地の検証の絞り込み（10-07、ADR-t1925-1）、cmuxとbrokerの撤去（10-08〜10-13）、fresh_sessionの導入（10-18〜19）、toolchain 1.99.1（10-25）が重なる。e2eの関門の失敗は10-03〜10-08に13回あり、10-08より後は0回。この期間のworkの中央値は前と比べない。

## 5. 再現の方法

1. 境界: `dagq events --task 966 --kind run_integrated --full`（event 40839）と、`dagq events --task 1239 --kind run_integrated`（event 56724）。
2. 前と後: `dagq stats --since 2026-09-26T00:00:00Z --until 40839 --full`、`dagq stats --since 40839 --until 56724 --full`、`dagq stats --since 56724 --full`。`runs[]`のうち`status`が`integrated`で`areas`に`runtime`を持つものを、`runs[].e2e`で群に分け、`work_breakdown.secs.e2e`・`work_breakdown.commands.e2e`・`work`・`land_phases.verify`・`land_phases.e2e`を集める。
3. 重なり: `dagq marks --since 2026-09-26T00:00:00Z`、`dagq events --kind update_failed --since 2026-09-28 --full`（payloadの`stage`が`e2e`のもの）、`dagq kpi --area runtime --compare 40839 --window 3`。
4. globの傾向: `git log main --since=2026-09-28 --format='%H %(trailers:key=Dagq-Run,valueonly)' --name-only`で着地のcommitの差分をrunに対応させ、`dagq.toml`の`[e2e] paths`に照らす。
