---
id: adr-t5039-1
type: adr
title: ITを本数でなく着地の検証のITの段の時間の予算で縛り、境界だけを残し、1本の天井とproductionの待ちを縮める口と広いfileの扱いを決める（ADR-t1707-1決定1をamends）
status: accepted
created: 2026-11-18
updated: 2026-11-18
accepted_on: 2026-11-18
amends:
  - adr-t1707-1 decision 1
amended_by:
  - adr-t5267-1
owners:
  - hisamekms
tags:
  - testing
  - performance
  - integrate
related:
  - adr-t1410-1
  - adr-t3802-1
  - adr-t3841-1
  - adr-t2895-1
  - adr-t1925-1
  - adr-t2892-1
  - adr-t3920-1
  - adr-t3761-1
  - adr-t5020-1
  - plan-it-time-budget
  - development-testing
  - development-task-registration
---

# ADR-t5039-1: ITを時間の予算と1本の天井で縛り、境界だけを残す

## Context

人はunitの方針の直後にITも本数でなく時間で調整すると決めた（request 327、goal 298）。
unitの予算と層ごとの上限を決めた[ADR-t5020-1](2026-11-16-t5020-1-unit-tests-are-bound-by-a-time-budget-and-per-layer-limits-with-a-volume-layer.md)と同じ形で、ITの狙う先と縮め方を決める。

[着地ログの測定](../plans/it-time-budget.md)は、入れ替え待ちの口を足した後の最新2件と、指定の比較列4844を持つ（6並列、他のrunと同時の負荷の下）。

| 記録 | IT Summary秒 | CPU合計秒 | p50秒 | p99秒 | IT / UT実行本数 |
| --- | --- | --- | --- | --- | --- |
| 4984前の4844 | 1436.173 | 7413.261 | 1.838 | 30.639 | 28.03% |
| 4984後の5067 | 1474.352 | 8720.761 | 1.985 | 25.157 | 30.13% |
| 4984後の5177 | 1189.819 | 7134.813 | 1.856 | 24.196 | 31.15% |

CPU合計はOSのCPU使用時間でなく、待ちを含むtestのPASS秒の和である。
全件選択でも既知の失敗などの除外があり、4844は途中で止まった集合なので、全登録数の時間や待ちの口だけの効果とは解釈しない。
4844は5秒超の475本がCPU合計の約80%、30秒超の24本が約26%を持ち、最新の5067も5秒超が約83%を持つ。
軽い本数を減らすより、重いtestの境界の取り違えと待ちを直す必要がある。

今の決定はfrontmatterのamended_byを辿ったものを使う。
[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)の配線と行き先は[ADR-t3841-1](2026-11-01-t3841-1-supervisor-wiring-tests-run-in-process-on-a-memdb-queue-with-port-fakes.md)、SQLiteのadapterの例外は[ADR-t2895-1](2026-11-01-t2895-1-sqlite-adapter-tests-in-infrastructure-open-sqlite-in-place.md)、着地の選択は[ADR-t1925-1](2026-10-07-t1925-1-landing-verifies-unit-tests-and-selected-integration-tests-and-ci-is-the-final-gate.md)とその後継の部分決定による。
選択の今の形は[ADR-t2892-1](2026-10-20-t2892-1-landing-it-leaves-added-sources-and-broad-entries-to-ci.md)、[ADR-t3761-1](2026-10-31-t3761-1-landing-it-selects-the-modules-a-changed-test-helper-reaches.md)、[ADR-t3920-1](2026-11-01-t3920-1-landing-it-keeps-using-an-old-table-and-widens-the-files-it-does-not-know.md)、その表の生成を変えたADR-t3926-1とOSの欠けを補うADR-t3927-1を含む。
[ADR-t1707-1](2026-10-05-t1707-1-time-gate-for-added-or-changed-integration-tests.md)はADR-t4948-1でlibへも広がったが、本ADRがamendsするのは決定1のITの天井と対象だけである。

## Decision

1. **ITに残すのはin-processで確かめられない境界だけにする。** process・socket・実のGit・launchd・ファイルの置き場などを代表の1 caseで確かめる。
   判断は副作用の無い関数のunit test、supervisorの配線は`src/compose`の`#[cfg(test)]`の配線のtestの層に置く（goal 239）。
   ADR-t1410-1の今の決定と[ADR-t3802-1](2026-10-31-t3802-1-integration-test-boundaries-in-a-named-registry-with-a-module-ratchet.md)の境界の登録簿・moduleのラチェットを保ち、時間の予算の側から言い直すだけである。
   減らすITの確かめていた中身はunit test・配線のtestの層・残すITに対応づけ、行き先の無いまま消さない。
   本goalでは登録簿に新しいidを足さない。

2. **本数でなく、ITの段を全部流しても壁時計10分以内、`NEXTEST_TEST_THREADS=6`でCPU合計60分（3600秒）以内の予算で縛る。**
   - unitの段の5分（ADR-t5020-1決定1）と合わせて15分にし、着地の検証の3600秒の上限にbuildや他の検査の余裕を残す。
   - 比べるのは本番の着地の検証のログのITの段のSummary秒と1本ごとの秒の和で、負荷の下の秒を使う。除外や中断がある記録だけで全件の予算達成としない。
   - Contextのとおり合計の大半は5秒超が持つ。本数を目標にして軽い境界のtestを削っても予算に近づかない。
   - 予算そのもので着地を止める関門は足さない。夜の`it-budget`の`.config/it-budget.toml`の`secs`はCIのrunnerで今の値を超えないための天井で、予算そのものではない。ITを減らすたびに夜の測定から予算の方へ下げ、増え方は夜のfinding、予算との差は本番の着地ログで見る（ADR-t5020-1決定1と同じ扱い）。

3. **1本の目標をp50 1秒・p99 10秒とし、全てのITに30秒の天井を置く（ADR-t1707-1決定1をamends）。**
   - 足した・変えたITの時間の関門は5秒のままにする。30秒の天井は差分で変わらないITにも当て、超えてよいのは`.config/it-slow-allow.toml`と`.config/it-slow-allow.d/`の理由つきの許可の一覧に載るものだけにする。天井と等しい秒は超過でない。
   - 許可の理由はADR-t1707-1決定2の今の読み方を保つ（守る境界、または移す予定のtaskかgoal。配線の既存項目は移し替えまでの扱い）。判断のcaseを並べただけでは許さない。
   - helperやproductionの待ちだけで既存のITが延びる決定6(1)の限界は、毎晩の測定のtestごとの天井（task 4987の`check-it-budget.sh`）で見つける。夜の秒と本番の秒の環境差は区別する。
   - 関門の配置の決定3とintegrateに段を足さない決定5は保つ。5秒の差分の関門を全件実行へ広げるのでなく、全件の30秒の天井を夜の既存の測定へ載せる。

4. **testが当たるproductionの待ちは、fixtureから縮められる口を持つ。** 入れ替え・pollの間隔・停止の猶予・socketの時間切れが当たる。
   - 先に、境界を要しない待ちを配線のtestの層へ移せないか考え、口を足さずに済むものを移す。
   - 形はtask 4984の`src/application/queue_service.rs`の`REPLACEMENT_WAIT_ENV`と`replacement_wait_from`（`DAGQ_REPLACEMENT_WAIT_MS`）を一般化する。口はenvとし、名前・既定・意味を定義のそばのdoc commentに書く。
   - 値から待ちを決める判断は副作用の無い関数にし、unit testで口なし→既定、口あり→その値、不正な値→既定を確かめる。既定を上限とする口は、その範囲も判断のtestで確かめる。
   - productionは口を渡さず既定のままにする。`tests/it`のbinaryは読み込みの時にfixtureから短い値を受け、実の待ちそのものを確かめるITだけが自分の呼び出しのcommand/envに長い値を渡す。in-processの呼び出しも渡す環境の値を使い、testがprocess全体のenvを変えない。
   - productionの待ちの既定・event・payload・ask・文面の意味を変えない。短い値でも境界の完了を条件で待ち、固定sleepで成功を決めない。

5. **本数の目標は置かない。** ITはUTの1〜2割を目安にするが、関門にしない。
   測定の今の比は28.03〜31.15%で、UTの分母やITの除外が異なる実行集合の比である。
   目安に近づけるためだけに境界の代表を消したり、unitを増やしたりしない。

6. **対応表で選ぶ今の規則を保ち、hubのfileを責務で分けて表の当たりを狭める。** ADR-t1925-1・ADR-t2892-1・ADR-t3920-1・ADR-t3761-1と上記の部分決定を変えず、`scripts/landing-it.py`の`choose`は変えない。
   - 予算に届けば全部流してもunitと合わせて上限内なので、全部流す条件は変えないのを基本とする。予算に届くまでの間も、共通のbuild入力は全件、古い表は2段の古さと広げる規則のままにする。
   - 測定の4984後の2件では、共通のmigrationが1件、絞った見込みの閾値が1件である。unitとITのSummaryの合計は2473.246秒・1614.105秒で3600秒未満（build等を含む保証ではない）。閾値は旧4844でも理由だが、広い`tests/common`のhelperと古い表の理由はこの標本には無い。頻度を全着地の割合とは読めない。
   - この秒と頻度ではCIへ任せる範囲を広げる根拠が足りず、閾値も広いhelperも現状を保つ。goal 293の4988はbuild等も含む検証全体と理由別の頻度を測り、上限に収まらない原因が閾値や広いhelperなら、その理由に限ってCIへ任せる範囲を広げる判断の材料にする。事故をCIが最終の関門として拾うADR-t1925-1の約束は保つ。変更を決める場合は対応するADRをamendsし、chooseと`--self-test`を変える別taskにする。
   - hubは対応表がITの25%を超えて写すsrcのfile（測定の一覧）とする。広い項目からは選ばずCIに任せるADR-t2892-1決定2を保つ。全ITが通るtransportと要求ごとの処理を別のfileへ分け、要求の変更がtransportの全件の対応に隠れないようにする。名前だけの分割やmoduleの付け替えでなく、共通経路と個別の処理の責務を分け、生成された表の対応で狭まったかを見る。
   - 最初の対象は`src/infrastructure/queue_service.rs`（1731/1835本、socket/clientとserviceの共通経路）と`src/application/queue_service.rs`（1494/1835本、要求ごとのuse case）とし、fileごとのrefactorをfollow_upにする。351 file全てを機械的に分割せず、共通経路を分けて再測定し、まだ個別の変更を隠すhubへ進む。

## Alternatives

- **本数を上限にする**: 軽い境界を削る動機になり、CPU合計を持つ遅いITを直せない。
- **全ITを5秒の差分の関門で流す**: workerの範囲を広げ、既存の遅さで無関係な着地を止める。既存の延びは夜の30秒の天井で見る。
- **待ちの既定をproductionでも短くする**: 本番の猶予やtimeoutの意味を変える。fixtureだけが口を渡す。
- **閾値・広いhelper・古い表の全件選択を一律にやめる**: 標本に無い理由まで扱いを変える根拠が無い。選択は保ち、境界への移し替えと待ちで全件の秒を予算へ近づける。

## Consequences

- goal 239の残りのtask 4543〜4567は止めずにそのまま流す。moduleごとの配線のITを配線のtestの層へ、判断をunit testへ移すことが決定1・2・5の主な手段になる。4544の境界の種類「supervisorの配線」の退役は移し替えの後のままにし、登録簿と許可の項目を行き先とともに整理する。移したlibのtestはADR-t5020-1の層ごとの上限を守る。
- goal 293の4985（長いITの理由と待ちの口）は止めずに流し、口の形を決定4へ揃える。既存の対象以外の重いmoduleにも同じ形を広げる後続taskをfollow_upにし、4985が扱う口にも未適合が残ればその形へ直す範囲を含める。
- 4986（drainのIT）はそのまま流す。実のprocessのdrainだけITに残し、判断と配線の行き先を対応づけ、実の猶予を確かめる呼び出しだけ長い値にする（決定1・3・4）。
- 4987（夜のtestごとの天井）はそのまま流し、その後の天井と許可の一覧の合わせをfollow_upにする。全ITの30秒と決定3の理由に揃え、`check-it-budget.sh --self-test`で30秒と超過と許可の有無を確かめる。夜の合計の天井は決定2の漸減と環境の区別に従う。
- 4988（範囲の広い変更の着地の検証の秒）はそのまま流す。決定6の理由別の頻度と上限の余裕の観測を選択の再判断に使い、全件選択の理由とhubを混同しない。
- chooseの変更は今回決めないので実装のfollow_upは作らず、hubの2 fileの分割はそれぞれrefactorのfollow_upにする。本taskではscriptsとsrcを変えない。
- 規則の本文は[testの制約](../development/testing.md)の「ITの段の時間の予算と待ちの口」と「CI」、登録は[taskの登録](../development/task-registration.md)が持つ。全ての着地の後、人かinboxが本番の着地ログの600秒・CPU合計3600秒との比較を残す。workerは本番で測定しない。
