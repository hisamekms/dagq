---
id: adr-t1925-1
type: adr
title: 着地の検証をfmt・clippy・レイヤーの検査・taskの軽い検査・unit test全件・影響範囲で絞ったITにし、coverageの関門と最終関門をCIにする。切り替えは設定で入れ、runtimeが設定に従ってtaskのcoverageの関門のコマンドを置き換える（ADR-0076を置き換え、ADR-0049決定1・ADR-t1410-1決定7をamends）
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
supersedes:
  - adr-0076
amends:
  - adr-0049 decision 1
  - adr-t1410-1 decision 7
owners:
  - hisamekms
tags:
  - runtime
  - integrate
  - testing
  - ci
related:
  - adr-0049
  - adr-0076
  - adr-t768-1
  - adr-t828-1
  - adr-t1410-1
  - adr-t1920-1
  - adr-t598-1
  - adr-t1942-1
  - plan-landing-it-selection
  - design-supervisor-lifecycle-integrate
  - design-supervisor-lifecycle-validation
---

# ADR-t1925-1: 着地の検証を軽くして影響範囲で絞ったITにし、coverageの関門と最終関門をCIにする

## Context

着地の検証は1つのslotで直列に流れ、その大半は全部のtestを流すcoverageの関門（[ADR-0076](0076-run-the-coverage-gate-tests-with-nextest.md)）の時間である。
testの時間のほとんどは統合テスト（IT）で、unit testは数%しか使わない（[ITの削減](../plans/it-reduction.md)）。
着地の数の上限はこの時間で決まり、混む時間帯はslotが埋まり続ける。
人は着地の検証を軽くして最終関門をCIにし、CIで分かる壊れは後から直すと決めた（request 43、goal 157）。
前提の2つは着地済みである。

- [ADR-t1920-1](2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md): supervisorがmainのCIを見張り、既に落ちているtestの一覧を持ち、修正taskのrunを見分ける。
- [着地のITの絞り込みの測定](../plans/landing-it-selection.md): CIの夜間の対応表（ファイル→IT）を過去の着地に当て、絞ったITの時間と見逃しを測り、全部流す閾値・共通のファイル・表の古さの上限を決めた。

置き換えとamendsの判断（[ADR-t1091-1](2026-09-30-t1091-1-amend-or-replace-by-number-of-decisions.md)）:

- ADR-0076は着地でcoverageの関門をどう流すかを決めたADRで、決定の大半（着地での関門のコマンド・登録済みのtaskの扱い・後続task）が変わるので丸ごと置き換え、まだ有効な決定（testの流し方・並列度の置き場・ツールの入れ方・CIの関門・遅いtestの見つけ方）は決定8に引き継ぐ。
- [ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)は決定1だけをamendsする。検証を`integrate`のrebase後の1回にすることは保ち、その1回で流すものがtaskの登録のままでなくなる点だけが変わる。run env・sccache・ツールの検査・claim順・statsの決定は変わらない。
- [ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)は決定7をamendsする。「coverageの関門と、integrateが全部のtestを流すこと」を変えないとした部分が変わる。判断をunit test、境界をITにする決定1〜6は変わらず、絞ったITが軽く済むのはその方針のおかげである。同じADRの退けた案（pathで流すtestを選ぶと誤って壊れたまま着地する）は、最終関門をCIにして事故を許容する人の決定で受け入れる。
- [ADR-t828-1](2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)はamendsしない。決めたのはcoverageの関門が覆う範囲（workspaceの全てのcrate）と旧い形のコマンドが有効なままであることで、どちらも変わらない（関門はCIでその範囲を見て、着地では新旧どちらの形も決定4で同じに置き換える）。
- [ADR-t768-1](2026-09-27-t768-1-rerun-failed-tests-once-and-land-again-on-flaky-only.md)はamendsしない。着地の検証のITの失敗を1回流し直して不安定なtestを見分けることは、絞ったITにもそのまま当てはまる（それがamendsしたADR-0076決定2は決定8に引き継ぐ）。

## Decision

1. **着地の検証は、fmt・clippy・レイヤーの依存の検査・taskに固有の軽い検査・unit test全件・影響範囲で絞ったITにする。**
   - unit testはrootのcrateとbrokerのcrateのunit testを全件流す。速く、判断の多くはunit testが確かめるため（ADR-t1410-1決定1）。
   - 行カバレッジの関門はCIだけで見る。CIのcoverageの計測つきの全testの実行はそのまま残す。着地でcoverageを計測しないので、計測つきのbuildとtestが着地のslotとworkerのCPUを取り合うことも無くなる。
2. **絞ったITは、CIの夜間の対応表の最新を使って差分から選び、次の2つを必ず含める。**
   - 差分で足した・変えたtestのファイルが定めるIT。表は差分の前のmainから作られているので、自分の足したtestを知らない。
   - 表に無いIT（表の生成の後に他の着地が足したか、名前が変わったtest）。差分のtestだけでは、表の生成の後に他の着地が足したtestを拾えない（[古さの材料](../plans/landing-it-selection.md#古さの材料)の近似で1日に約90本、2日に約190本）。測定は、表に無いITを必ず流すことを表の古さの上限を採る条件とした（[決めたこと](../plans/landing-it-selection.md#決めたこと段-3b-の-adr-と-design-が使う)の3）。
   - **ITを全部流す条件**（fallback）: 絞ったITの見込みの時間が上限を超えるとき、共通のファイル（多くのITに当たるファイルと、表が判断できないファイル）に触れたとき、表が取れないか古すぎるとき。上限・共通のファイルの一覧・古さの上限とそれを測る物差しは測定の「決めたこと」の1〜3に拠る。上限を秒でなくIT全部に対する割合で測るのは、表の秒が計測つきのCIの値で、着地のhostの秒と違うため。
   - 見逃しは最終関門のCIが拾う（決定5）。
3. **既に落ちているtest（ADR-t1920-1決定5の一覧）は着地の検証から外す。CIの修正taskのrunでは、直す対象のtestを外さない。**
   - 他人の壊れで着地が止まらず、workerが自分の変更でない失敗を直しにいかないため。mainが赤でも着地は続き、壊れはCIの見張りと修正taskが受け持つ。
   - 修正taskのrunの見分け方と、外さない範囲（そのfindingのtestだけ、他のfindingのtestは外す）はADR-t1920-1決定5に従う。
4. **切り替えは設定で入れ、登録済みのtaskの検証のコマンドは書き換えない。runtimeが設定に従って、taskの検証のコマンドのうちcoverageの関門を着地の検証に置き換える。**
   - 置き換えはrebase後の1回の検証の中で行い、他のコマンド（taskに固有の検査など）は登録のまま流す。coverageの関門を持たないtaskは何も置き換わらない。設定の無いrepositoryは今までどおり登録のコマンドを流す。
   - 置き換えと、着地の検証のコマンドに渡す材料（base commit、既に落ちているtestの一覧、修正taskのrunか）はruntimeの汎用の仕組みにする。何を置き換えるか、ITの選び方、表の取り方と置き場、共通のファイルの一覧は、このrepositoryの設定とscriptに置く。runtimeはこのrepositoryのtestの構成を知らない（plugin・runtimeの汎用性）。
   - 新しい設定は固定バイナリが読めるようになってから足す。
5. **最終関門はCIで、CIで分かる壊れは後から直す。**
   - mainの前でCIを通す仕組み（merge queueのようなもの）と、壊れた着地をrevertする手順は作らない。壊れはCIの見張りがfindingにし、修正taskで直す（ADR-t1920-1決定4）。coverageの関門で落ちたCIも、落ちたjobとstepのfindingとして同じ経路に乗る。
   - 自動更新（固定バイナリの入れ替え）にCIの確かめや全testを足さない。自動更新は着地とほぼ同じ頻度で走り、足せば着地の検証を軽くした意味が無くなる。事故は許容する。
   - 全部のe2eの関門（着地の前と自動更新・installの前）と、固定バイナリを前のものに戻す手順は変えない。
6. **workerの手元の検証の規則は変えない。**
   - workerは今も全部のtestとcoverageを手元で流さず、変更に関係するtestとstressだけを流す。着地の検証が軽くなっても、workerに求める範囲は同じで足りる。
   - 着地の検証が落ちてresumeされたrunが、落ちたコマンドを手元で流して再現してよい例外も同じで、再現するのは置き換えの後の着地の検証のコマンドになる。
   - 既に落ちているtestをworkerのpromptとreviewの材料でどう見せるかと、自分の変更が原因でなければ直さない規則は、このADRでなくgoal 157の後の段が決める（一覧をそれらの材料に使うことはADR-t1920-1決定5）。
7. **差分のarea（変更の範囲）からtaskの検証を選ぶ案（goal 70、draft）とは別にし、その決定を先取りしない。**
   - このADRが選ぶのはITの本数だけで、選ぶ材料は対応表と差分のファイルである。taskの検証のコマンドの選び方、changeとareaからの推奨の組み合わせ、置き換えをareaで変えるかは決めない。goal 70が後で決めるときは、このADRの置き換えの仕組みを使っても、変えてもよい。
8. **ADR-0076からまだ有効な決定を引き継ぐ。**
   - ITとunit testは、testを1件ずつ別のprocessでbinaryをまたいで並列に流すtest runnerで流す。着地の検証の並列度はrepositoryの設定がworkerと検証に渡すrunの環境（ADR-0049決定3）に置き、test runnerの設定ファイルには書かない（CIと人の手元を絞らないため）。
   - 落ちたtestを1回流し直して不安定なtestを見分けることはADR-t768-1に従う。
   - test runnerは人がhostに入れ、workerは入れない。無いhostでは着地の検証が失敗してrunは人に返る。
   - CIのcoverageの関門も同じtest runnerで流し、workspaceの全てのcrateを合わせた行カバレッジで落とす（ADR-t828-1）。
   - 遅いtestはtest runnerが印を出し、testごとの時間から削るtaskを登録する。

## Alternatives

- **着地で全部のtestとcoverageを流し続ける**: 漏れは無いが、着地のslotが1本のまま時間の大半をITとcoverageの計測に使い、着地の数の上限が上がらない。人の決定が退けた。
- **coverageだけを外し、ITは全部流す**: ITがtestの時間のほとんどを占めるので、縮む幅が小さい。
- **差分のファイル名の規則でITを選ぶ（対応表を使わない）**: 表は要らないが、中心のファイルが多くのITに当たることを規則で書けず、選びすぎるか漏らす。表はtestごとのcoverageで実際の当たりを持つ。
- **差分で足した・変えたtestだけを足し、表に無いITは流さない**: 表の生成の後に他の着地が足したtestが選ばれず、古さの上限を採った前提が崩れる（決定2）。
- **既に落ちているtestを外さない**: mainが赤の間、全ての着地が他人の壊れで止まり、workerが自分の変更でない失敗を直しにいく。
- **mainの前でCIを通す（merge queue）か、壊れた着地をrevertする**: 壊れはmainに入らないが、着地がCIの時間を待ち、人の決定（壊れは後から直す）に反する。
- **自動更新にCIの確かめか全testを足す**: 固定バイナリは守れるが、着地とほぼ同じ頻度で走るので、軽くした時間がそこで戻る。e2eの関門と前のものへの戻しで足りるとする。
- **登録済みのtaskの検証のコマンドを書き換える**: 人とplan reviewの外でtaskの検証を変えることになり、書き換えの漏れも出る。設定での置き換えなら1か所で入れて戻せる。
- **ITの選び方をruntimeに持つ**: このrepositoryのtestの構成と表の形をruntimeが知ることになり、汎用でなくなる。

## Consequences

- 着地の検証は数分になり、着地の待ちはほぼ消える見込み（推定）。前後の比較はgoal 157の段4が`docs/plans/`に残す。
- 壊れたcommitがmainに入りうる。CIの見張りと修正taskが直し、その間は既に落ちているtestの一覧が他の着地を守る。壊れが固定バイナリに入る事故も許容し、e2eの関門と前のものへの戻しで受ける。
- 置き場の分担（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)決定3・4、[ADR-t1942-1](2026-10-07-t1942-1-design-docs-in-four-layers-with-size-budgets.md)決定2〜4）:
  - このADRは方針・境界・退けた案と理由を持ち、値・一覧・設定のkey・envの名前・コマンドとscriptの名前を持たない。
  - 予定の姿の流れ・責務と境界・不変条件は[integrate](../design/supervisor-lifecycle/integrate.md)と[Validation](../design/supervisor-lifecycle/validation.md)が持つ。
  - 測定値と値の根拠は[着地のITの絞り込みの測定](../plans/landing-it-selection.md)が持つ。
  - 設定の節・key・型・既定値と、runtimeが着地の検証のコマンドに渡すenvの名前は、後続のruntimeの実装が定義のそばのdoc commentで定義する。
  - 閾値・共通のファイルの一覧・古さの上限の値は、後続のconfigの実装が`dagq.toml`とscriptsに置き、測定を根拠としてlinkする。
  - taskの登録の推奨の組み合わせは後続のtaskが新しい形にする。
- このrepositoryで切り替わるまでは、着地の検証は登録のまま全部のtestとcoverageを流す。
