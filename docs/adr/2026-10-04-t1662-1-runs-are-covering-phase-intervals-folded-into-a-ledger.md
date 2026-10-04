---
id: adr-t1662-1
type: adr
title: runとtaskの一生を、supervisorが工程を移るときに記録する重ならず全体を覆う区間の列（タグblocker・holds・attemptつき）で表し、1つの畳む関数で台帳（run・task・session・queue・nodeの行）に畳み、統計は台帳だけを1つの共通の読み取り関数で読み、終端はpushの終わり、遅れて届く入力は畳み直し、切り替え前のrunは旧方式の値を凍結して残し、claimの時の属性はキーと値の開いた組で運ぶ（ADR-0049決定5・ADR-0051決定1をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-0049 decision 5
  - adr-0051 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - observability
  - stats
related:
  - adr-0048
  - adr-0049
  - adr-0051
  - adr-t813-2
  - adr-t980-1
  - adr-t1233-1
  - adr-t1486-1
  - adr-t1545-1
  - adr-t1662-2
  - adr-t1662-3
  - design-measurement
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-kpi
  - design-supervisor-lifecycle-timeline
  - design-supervisor-lifecycle-first-commit
---

# ADR-t1662-1: 計測のモデル（区間・タグ・台帳・終端・旧方式・runの比較の軸）

## Context

直近24hに着地した67 runのclaim→着地は中央値46分・p90 340分で、合計109hのうち`wait_to_land`が78h（exit 21.6h・resume 14.2h・verify 13.1h・ask 11.1h）を占めた（request 13、2026-10-04）。exitの多くは誤分類で、`stats::landing::LandClock`はaskの間に他の工程のeventが来ると開いているaskを忘れ、`approve_landing`の答え待ちがexitに入る（task 1400で345分、1462で318分）。`startup`は非対話のworkerでは`work`とほぼ同じ値になり、`work_breakdown`（worker・resume・reviseの和）は`work`（workerだけ）と合わない。runの工程はsupervisorの`Phase`・`domain::run_progress::Phase`・`LandClock`・timelineの空白の理由・`domain::forecast::Phase`が別々に組み立てていて、その食い違いが誤りの元である。

人は2026-10-04に、個別の直し（exit・startup・work_breakdown）をせず計測を作り直すと決め、inboxと決めた方針（決定の記号M・L・D・E）を「おすすめの形でok」と承認した。ADRを3本に分けることも人の決定（D10）で、このADRは計測のモデル（M1〜M8）・層と責任（L1〜L6、D7）・切り替え（D3・D4）を持つ。ストアの抽象化（D1・D2・D6）は[ADR-t1662-2](2026-10-04-t1662-2-measurement-stores-ssot-and-views.md)、実行する側と送る口とコマンドの分類（D8・D9・E1〜E7）は[ADR-t1662-3](2026-10-04-t1662-3-executors-send-telemetry-and-commands-are-classified-by-shape.md)が持つ。eventの欄・列とJSONの形・関数の名前・数値は[計測の設計](../design/measurement.md)が持つ。

## Decision

### I. 計測のモデル

1. **M1 区間の列。** runの一生を、重ならず隙間なく全体を覆う区間の列で表し、区間の長さの合計を所要時間に一致させる。どの工程にも当たらない時間はunattributedの区間として出し、他の区間に配らない。unattributedの目安は所要時間の2%未満で、KPIが見張る。超えるのは記録の抜けの印で、直すのは記録の側である。
2. **M2 境目はsupervisorが記録する。** supervisorは工程を移るたびに`run_phase_changed {phase, blocker, holds, attempt, cause, v}`を記録し、読み手はほかのeventから境目を推さない。区間の列に畳むのは1つの畳む関数で、台帳を作る係と、走っているrunを見る読み手（status・timeline・forecast）が共有する。
3. **M3 タグで集計する。** 統計は工程の名前ではなくタグで集計する。`blocker`は何が進みを決めているか（`queue`・`ai`・`compute`・`human`・`runtime`・`external`・`infra`）、`holds`は握っている枠（`worker_slot`・`landing_slot`・`none`）、`attempt`はrunの中の何回目の試みか。
4. **M4 工程の名前は自由。** 工程を足す・分ける・名前を変えるのに統計の変更は要らない。各工程のタグは`Phase::tags()`で工程の定義の隣に置き、付け忘れはコンパイルで止める。
5. **M5 物差しは2本。** 所要時間と、slotを握った時間（`holds`が`none`でない区間の和）。資源（M8）は区間に添える量で、物差しではない。
6. **M6 範囲はtaskの作成からpushまで。** taskの台帳は`task_created`から始まる。claimの前の待ちの理由は、supervisor単位の区間`slots_full_started`/`slots_full_ended`・`claim_held`/`claim_resumed`・`claim_deferred`/`deferral_ended`をtaskの区間に重ねて読む（taskの区間の列は分けない）。`run_claimed`にreadyになった時刻・候補の中の順位・候補数を足す。
7. **M7 sessionのstep。** agentのsessionの中身（turn・tool・コマンド）はstepとして区間の内側の層に持ち、親の区間の列を分けず合計を変えない。stepを送る側と置き場はADR-t1662-3とADR-t1662-2が決める。
8. **M8 待たせた相手と資源。** 着地slotの待ちの区間には、その間に着地slotを握っていたrun（`blocked_by`）を記録する。区間ごとの資源、nodeの資源の時系列、runが付いた実行環境（`environment_attached`）も記録し、区間に結びつけて読む。

### II. 層と責任

9. **L1 記録→台帳→統計。** 記録（EventStore・StateStore、とSessionStepStore・NodeSampleStore）、台帳（LedgerStore）、統計（ReportStoreなど）の3層にし、上の層は下の層だけを読む。各ストアの区分（SSOTかビューか）はADR-t1662-2が持つ。
10. **L2 台帳の形。** 台帳は対象ごとに1行で、区切りの値（区切りの時刻・所要時間・slotを握った時間・タグごとの合計・unattributed）は列、フローで形が変わるもの（区間の列・stepの要約・attemptごと・属性の組）はJSONにし、行は`ledger_v`・`inputs_through`・`final`を持つ。行の種類は次の5つ（task 1682で足した契約）で、どれもEventStore（とSessionStepStore・NodeSampleStore）だけから畳む。
    - **run**: runの一生。そのrunの上のsessionとjobの項目と、Execution（[ADR-t1486-1](2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)）のtokenの列も持つ。
    - **task**: `task_created`から`completed`か`canceled`まで。
    - **session**: runを持たない対象のsessionとjobの1回（runtimeのplanner・plan review・goal review・throughput review・observerのjob・inboxと人の対話の区切りなど）。runの行と同じExecutionのtokenの列を持ち、対話のsessionのtokenはその区切りごとの行に畳む。
    - **queue**: run・task・sessionに属さないqueue全体の区間と出来事（`slots_full`・`claim_held`・`claim_deferred`、着地slotの使用、`update_installed`・providerの切り替えなど）。
    - **node**: nodeごとの資源の時系列を時間の区切りで畳んだもの。今のhostの負荷の値（statsのhost、kpiのhost・`cpu_per_landing`・`load_per_core`・`health.disk`、reportのhost）の置き場（task 1685・1686）。
11. **L3（D7）台帳を作る係。** supervisorの周回が終わった後に、行が無い・`ledger_v`が古い・`inputs_through`より後に入力が届いた（dirty）対象を畳み直す。同じ入力からは同じ行を作る（冪等）。係は観測と分析のcontext（[ADR-t1545-1](2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)、goal 100のmodule）に置き、`Supervisor`の本体に状態を足さない。
12. **L4 統計は台帳だけを読む。** stats・kpi・reportの全ての欄（hostの欄、run・taskを持たない`jobs`・`sessions`などの集計を含む）は、5種類の行を今のstatsの窓と絞り込みで選んで返す1つの共通の読み取り関数だけから作る。読み手は`LedgerStore`とこの関数だけを通す。
13. **L5 台帳は制御に使わない。** 台帳は計測専用で、runtimeの判断（claim・slot・resume・着地）は台帳を読まない。
14. **L6 恒久のストアだけから作る。** 台帳と統計はEventStore・SessionStepStore・NodeSampleStoreだけから作り、StateStoreと材料（run dir・log・CSV・transcript）を読まない。
15. **tokenの集約はtask 1494が持つ。** tokenの集約（Executionの終わった時刻での日への振り分けと、actor × provider × model）はgoal 95のtask 1494が決め、台帳はその入力（Executionと対話の区切りの記録）の置き場だけを持つ。runを持たない対象のstepをtokenの代わりにしない。

### III. 終端・台帳の確定・旧方式

16. **(i) 終端はpushの終わり。** `run_integrated`の後は工程`push`（`run_integrated`と同じtransactionで記録）で、`push_finished`（`already_delivered`を含む）か`push_skipped`で閉じる。`push_failed`は工程`push_pending`（`blocker: human`）にし、同じremote・branchへの後の`push_finished`で閉じる（mainは直線なので後の着地のpushが前の着地のcommitを含む）。人が手で打った`git push`はeventが無いので、次の`push_finished`まで`push_pending`のまま。着地しないrunは失敗・取り消し・interruptedで閉じる。taskは`completed`（そのrunのpushの終わり）か`canceled`で閉じる。今のstatsの値（`work`・`validate`・`wait_to_land`など）は今の意味のまま（`wait_to_land`は`run_integrated`まで）。
17. **(ii) 台帳の確定と遅れて届く入力。** 行は畳んだ入力の到達点`inputs_through`（EventStoreのevent idと、stepとnodeの時系列の受けた順のid）と`final`の印を持つ。`final`はrunが終わり（`push_pending`でない）、そのrunのtelemetry専用のtokenが失効し、supervisorの資源の最後の記録が済み、猶予が過ぎてから付ける。`final`の後もdirtyなら畳み直す。
18. **(iii)（D3）旧方式の値。** 切り替え前のrun（`run_phase_changed`が無い）は区間に畳み直さない。旧方式の印と、今のstatsのrunごとの値（`work`・`validate`・`wait_to_land`・`land_phases`・`startup`）を行のlegacyのJSONに一度だけ書いて凍結し、`ledger_v`を上げても作り直さない。共通の読み取り関数は、新しい行は区間から、旧方式の行はlegacyのJSONから、同じ名前と形の値を返す。timelineは旧方式のrunの空白の理由を作り直さずlegacyとする。
19. **D4 今の値は1期間だけ残す。** `work`・`wait_to_land`・`land_phases`・`startup`は、1期間（段3の後。長さは設計書）は台帳から同じ名前で作り、その後廃止する。`startup`は`first_commit`に改名する。CLIの既存のJSONの項目はその期間は同じ名前で残す。
20. **置き換えるものと、しないこと。** `LandClock`・supervisorの`Phase`・`run_progress::Phase`・`forecast::Phase`・timelineの空白の理由の別々の組み立ては、`run_phase_changed`と畳む関数で置き換える。exit・startup・work_breakdownの個別の直しはしない。

### IV. runの比較の軸（request 17、2026-10-04に人が承認）

21. **claimの時の属性はキーと値の開いた組。** `run_claimed`はrunの属性を名前を決めないキーと値の組（`attributes`）で持ち、記録・台帳（runの行のJSON）・統計（`kpi --by <キー>`と`--compare`の層）はキーを知らずに運ぶ。新しい軸はclaimの時に属性を1つ足すだけで、台帳とkpiに手を入れずに読める。値は文字列、キーは小文字と`_`と`.`だけ、キーと値と組の大きさに上限を置く（値は設計書）。同じキーの意味を変えず、変えるなら新しいキーにする。後の指示の版のhashとA/Bの群はこの属性として足す。
22. **(α) 属性はclaimの時点の生の値だけ。** キーは`build`・`claude_version`・`codex`・`rustc_release`・`rustc_host`・`parallel`・`slots`・`load_avg`・`requested_provider`・`claim_provider`・`route`・`claim_model`・`effort`・`trial_group`（task 1664の(d)）。
23. **(β) 派生の軸は予約名。** `provider`・`claude`・`model`・`group`・`slot`・`load`・`toolchain`（と`change`・`area`・`nature`）は属性のキーにしない予約名で、kpiが属性とclaimの後の事実から今の`axis_value`と同じ規則で作る。`provider`は最後のprovider（[ADR-t813-2](2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定7）。Codexで作業したrunは`claude=none`、`model`は最初のCodexの`turn_finished`が報告した実モデルで、`trial_group`を捨てる。`group`は`trial_group`、無く`effort`があれば`none`、どちらも無ければ`unknown`。`slot`と`load`は帯、`toolchain`はcargo専用。
24. **(γ) 同じ名前のキーの読み取りの優先。** kpiの層のキーが派生の軸の名前なら派生の値を使い、属性に同じキーがあっても使わない。それ以外は属性の値、無ければ`unknown`。`build`・`codex`・`parallel`・`route`・`effort`は派生しない軸で、属性のキーと層の名前が同じで、値も今と同じ文字列。今のkpiの`Axis`の名前（`build`・`parallel`・`slot`・`load`・`toolchain`・`claude`・`provider`・`route`・`codex`・`group`・`model`・`effort`）は`kpi --by`の同じ名前の層として残す。
25. **(δ) claimの後の事実は畳む関数が運ぶ。** 最後のprovider・Codexで作業したか・最初のCodexの`turn_finished`のmodelは、runの畳む関数（task 1666）が`run_claimed`の`provider`・`provider_switched`の`to`（claimの前に記録されたclaimでの移りを含む）・`turn_finished`の`provider`と`model`から、今のstatsの`MeasureTrack`と同じ規則で作り、runの行に属性の組と別に持つ。claimの後に決まる軸（`change`・`area`・最後のprovider・`nature`）は今の別の欄のまま。
26. **(ε) 旧方式の行。** 旧方式の行は今の`RunMeasures`（finishの後の値）から属性の組とclaimの後の事実を一度だけ作って凍結する。

### V. 他のADRとの関係

- **[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定5をamends**: run単位の区間（作業・validating・着地待ち）と`wait_to_land`の工程別の内訳（「区切りのeventはruntimeのtaskが決め、既存の出力項目は変えず足すだけ」）を、決定1〜4の区間とタグ、決定19の1期間の後の廃止に改める。閾値超え・`--since`・observerの入力は変えない。「新しい表は持たない」の例外はADR-t1662-2が同じ決定をamendsで書く。
- **[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)は変えない**: sessionの区間（kind・開いている時間・稼働時間）は台帳の入力として残り、`sessions`の項目は台帳から同じ形で作る。決定11の「既存の出力項目を変えず足すだけ」はADR-0048が足した項目の規則で、決定19が廃止するのはADR-0049決定5が定めた値（`work`・`wait_to_land`・`land_phases`）と、最初のcommitの観測（goal 11の決定4）が足した`startup`である。
- **[ADR-t1486-1](2026-10-04-t1486-1-supervisor-records-token-usage-per-execution.md)は変えない（関係: 入力の置き場）**: Executionの記録はその決定どおりsupervisorが書き、台帳はrunとsessionの行のtokenの列にそれを畳むだけで、集約は決定15のとおりtask 1494が持つ。
- **[ADR-0051](0051-kpi-time-series-report-and-push.md)決定1をamends**: KPIの一覧の`phase.<工程>`（着地したrunの`startup`・`work`・`validate`・`wait_to_land`）と`land_phase.<工程>`は、決定19の1期間は台帳から同じ名前で作り、その後は所要時間とslotを握った時間のタグ（`blocker`・`holds`・`attempt`）ごとの値と`phase.first_commit`に置き換える。hostの設定の`[kpi.targets."phase.startup"]`などの目標の名前は同じ変更で読み替える（読み替えの対応は設計書）。unattributedの割合のKPI（このADRの決定1）を足す。台帳はrun_eventsを決まった規則で畳んだビューなので、KPIを「run_eventsから決まった規則で導く」ことと決定1のほかのKPIは変えない。

## Alternatives

- **exit・startup・work_breakdownを個別に直す**: 人が退けた。組み立てが5か所に残り、次の食い違いを生む。
- **読み手ごとにeventから工程を推す（今の形）**: 推し方が読み手ごとに分かれ、askの間の別のeventのような順序の違いで誤る。
- **工程の名前で集計する**: 工程を足すたびに統計が変わる。タグなら名前を自由にできる。
- **統計がeventを直接読む**: 読み手ごとに畳み方が割れ、旧方式と新方式の切り替えも読み手ごとになる。
- **切り替え前のrunも区間に畳み直す**: 境目の記録が無いので推すことになり、M2に反する。
- **claimの属性を決まった欄にする**: 軸を足すたびに記録・台帳・kpiの3か所を変えることになる。

## Consequences

- 段は0（このADR・ADR-t1662-2・ADR-t1662-3と設計書）→1（`run_phase_changed`ほかの記録）→2（ストアのport・台帳と畳む関数・1週間の並走比較）→3（読み手を台帳と畳む関数へ移し`LandClock`を撤去）。並走の間は今のstatsと台帳の差を全て説明する。
- 工程の境目が増えるたびに`Phase::tags()`を書く手間が増えるが、付け忘れはコンパイルで止まる。
- 切り替え前のrunは区間の内訳を持たず、旧方式の値だけで比べる。
- 台帳はビューなので、畳み方を変えたら`ledger_v`を上げて作り直す（旧方式の行を除く）。
