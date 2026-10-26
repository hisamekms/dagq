---
id: plan-spike-plan-baseline
type: plan
title: Spikeを含む計画の効果の指標（実装の着地までの総時間・前提の誤りによる手戻り・区分ごとのclaim待ち・plannerの枠の占有）の定義、過去の前例からの基準値、導入の後の比較の条件
status: active
created: 2026-10-26
owners:
  - hisamekms
tags:
  - measurement
  - planning
related:
  - adr-t1487-1
  - adr-t1487-2
  - adr-t947-2
  - adr-t808-1
---

# Spikeを含む計画の効果の指標と基準値

goal 96のacceptance (5)のうち「Spikeの時間を含む実装着地までの総時間・前提の誤りによる手戻り・区分ごとのclaim待ち・plannerの枠の占有の基準値と比較の条件」を置く文書。runtimeがSpikeを実行区分として知る前（[ADR-t1487-1](../adr/2026-10-04-t1487-1-spike-is-an-execution-class-with-a-judged-result-and-a-durable-replanning-request.md)・[ADR-t1487-2](../adr/2026-10-04-t1487-2-spikes-share-the-worker-slots-under-a-cap-with-cross-class-aging.md)の実装と、`dagq.toml`の`[supervisor] max_spikes`・`max_investigations`の適用の前）に、前提を先に確かめた計画を前例として選び、同じ数え方で基準値を出す。導入の後の評価はこの文書の3章の条件で別のtaskが行う。

数えたのは固定バイナリ`~/.local/bin/dagq`の読み取り（`events`・`stats`・`kpi`）だけで、本番のqueueの状態は変えていない。

## 要点

- **境界**: `max_spikes = 1`・`max_investigations = 2`をsupervisorが読んだ`supervisor_config_changed` event 184490（2026-10-13T16:53:31Z）と、その印の`mark_recorded` event 184541。前例はSpikeの最初のsubmitがこれより前のもの。
- **総時間（計画の着地）**: 前提の確認だけで終わらず実装まで着地した純粋な前例（P1〜P3）は、Spikeのsubmitから計画した実装の最後の着地まで18.0〜61.6時間（中央48.6時間）、人の答え待ちを除くと16.9〜52.5時間（中央48.5時間）。境界をまたぐP6は376.7時間（除くと343.3時間）。
- **結果が読まれずに止まった計画**: P4（goal 65）はSpike 3本が2026-10-02に着地した後、実装も中止も決まらず、2026-10-26の時点で24日あまりopenのまま。P1のgoal 51もopenのまま。goal 96の解く問題（再計画の仕事が人の記憶に頼る）の前例で、件数で数える（3章の判定の目安）。
- **前提の誤りによる手戻り**: 純粋な前例P1〜P4のSとIで、plan reviewが止めた前提の誤りは2件（runの前）、runに入ってから分かった前提の誤りは1件（P3の固定バイナリ）で、計画1件あたり0.25件。境界をまたぐP5・P6を含めるとそれぞれ8件と3件。runの手戻りの候補の大半（P6のS+Iで35件中34件）は実装の欠けと文書の照合で、前提の誤りではない。
- **区分ごとのclaim待ち**: 導入の前の前例では、Spikeの役のtaskの待ちの中央2.9時間（9件、p90 68.3時間）、実装の中央1.15時間（34件、p90 14.0時間）。本番全体の導入の前の実装の待ち（`ready_at`のある2026-10-08〜10-13の121件）は中央2.5時間・p90 47.5時間。
- **plannerの枠の占有**: 導入の前の14日（2026-09-29T16:53Z〜10-13T16:53Z）にruntimeのplannerのsessionは1,298本、開いていた合計173.1時間（`runtime_planners` 2枠の336時間×2のうち25.8%）、そのうち働いていた時間は34.9時間で、138.3時間は開いたまま働いていない。前例の計画に紐づくruntimeのplannerはどれも合計1.1時間以下で、前例の計画を立てたplannerは人が開いたsession（goalに紐づかず、計画ごとには数えられない）だった。

## 1. 指標の定義

計画は「goal Gと、その計画の前提を確かめるtaskの集まりS」で表す。導入の後はSを`execution_class`が`spike`のtaskとし、導入の前はtaskの題・説明から手で選ぶ（2章）。Gの残りのtaskを次に分ける。

| 集まり | 中身 |
| --- | --- |
| S | 前提を確かめるtask（Spike） |
| I（計画した実装） | Gのうち、Sの最初のsubmitの後（60秒の余裕。導入の前の計画はSとIを1つのproposalで出した）に最初のsubmitがあり、completedになったもので、plannerが登録したもの（originが無い）か、Sのtaskのfollow_upが登録したもの（導入の前の再計画はSpikeのfollow_upを経た） |
| F（その他） | 同じ期間に作られた、他のtaskのfollow_upとgoal reviewのgap |

### (a) Spikeを含む実装の着地までの総時間

- **計画の総時間**: Sの最初のsubmitから、Iの最後のcompletedまで。Spike・再計画・2度目のplan review・実装の待ちと作業を全て含む。Iが無いか、着地していないものは「open」とし、総時間を値として比べない（件数で数える）。
- **goalの総時間**: 同じ始まりから`goal_closed`まで（openなら数えた時点まで）。F（他のfollow_upとgoal reviewのgap）の分を含む参考の値。
- **人の答え待ちを除いた値**: 集まり（計画はSとI、goalはS・I・F）のtaskの`ask_opened`から最初の`ask_answered`か`ask_closed`まで（閉じていなければ区間の終わりまで）の区間の和集合を、総時間の区間に切り詰めて引く。askのkindは問わない（`approve_landing`・`worker_question`・`stalled`など、どれも人の手を待つ）。和集合なので、同時に開いたaskは1回だけ引く。
- **添える値**: Sの最初のcompleted、再計画の間（Sのどれかが着地した後にsubmitされたIのtaskについて、その直前のSの着地からsubmitまでの最短。IがすべてSより前にsubmitされていれば0）、Sと同時にsubmitされたIの数と後からのIの数。導入の後は、Spikeの着地から再計画の依頼のplannerが実装をsubmitするまでがこの間に当たる。

### (b) 前提の誤りによる手戻り

S・I・Fのtaskの次の5種を手戻りの候補にする。

| 候補 | 記録 | 判定の材料 |
| --- | --- | --- |
| plan reviewのrevise | `plan_review_finished`の`decision`が`revise`（plan reviewごとに1件） | `reason_codes`の全ての組 |
| runのreviewのreviseとconcern | `review_finished`の`verdict` | `reason_codes`の全ての組と`primary_code`、`summary`と`reasons` |
| failedのreceipt | `validation_finished`の`receipt.result`が`failed` | receiptの`summary` |
| workerの質問 | `ask_opened`の`kind`が`worker_question` | `topics`（[ADR-t947-2](../adr/2026-09-28-t947-2-worker-questions-carry-topic-codes.md)） |
| submitの後のcancel | submitされたことのあるtaskの`task_status_changed`の`to`が`canceled` | `duplicate_of`・理由・noteと、登録し直したtaskの有無 |

数え方の規則:

- **前提の誤り**: 計画が事実として置いた前提が、plan reviewかrunで偽と分かったもの。コードが`wrong_premise`・`acceptance_infeasible`・`precondition_missing`のどれかを含めば、読まずに前提の誤りとする。
- **読んで決めるもの**: 前提の誤りのコードを含まず、主のコード（runのreviewは`primary_code`、他は全てのコード）が無い（古いreview・askとfailedのreceipt・cancel）か`acceptance_unmet`のもの。理由の本文を読み、計画の前提が偽だったときだけ前提の誤りとし、読みをscriptの`JUDGED`に1行ずつ残す。記録から分からないものは「不明」として別に数える。
- **それ以外のコード**（`docs_drift`・`code_defect`・`local_slip`・`test_gap`・`repo_rule_violation`・`adr_conflict`・`adr_design_mismatch`・`verification_rule`・`incomplete_spec`など）は前提の誤りに数えない。`adr_conflict`は記録された決定との食い違いで、未確認の事実についての前提ではないので外す。
- 計画の後のproviderや環境の変化で通らなかったもの（2026-09-30のClaudeの停止でCodexのworkerが代行し、sandboxが`ps`やe2eを拒んだfailedのreceiptなど）は前提の誤りに数えない。
- 段で分ける: plan reviewの段（runの前に止まり、workerの時間を使わない）と、runの段（claimの後に分かり、作業か待ちが戻る）。Spikeが効けば、Iのrunの段の前提の誤りが減り、Sの中で結果として分かる（Spikeの不成立の判定は手戻りでなく結果）。
- 集まりで分ける: SとIを主に数え、Fは参考に添える。

### (c) 区分ごとのclaim待ち

- 各taskの最初の`run_claimed`（`from`が`ready`）について、taskがclaimできるようになった時刻からclaimまで。導入の後のclaimは`run_claimed`の`ready_at`（runtimeが`claimable_since`で決め、`dagq stats`の`claim_waits`が区分ごとに数える値）を使う。
- `ready_at`の無い古いclaimは、runtimeと同じ読み方で補う: claimの前の最後のreadyへの移り、各先行taskのcompleted、各goal依存の`goal_closed`のうち遅いもの。
- 区分: 導入の後は`execution_class`（無ければimplementation）。導入の前はSをspike、Iをimplementationとして手で分ける。

### (d) plannerの枠の占有

- runtimeのplannerのsession（`session_opened` / `session_closed`の`kind`が`runtime_planner`）の本数、開いていた時間の合計、働いていた時間（`session_closed`の`active_secs`）の合計、その差（枠を持ったまま働いていない時間）。
- 計画ごとには、Gを`goal_ids`に持つか、SかIのtaskに紐づき、計画の総時間の区間に開いたsessionを数える。
- 本番全体には`dagq kpi --since A --until B`の`session_open.runtime_planner`・`session_active.runtime_planner`を使い、`runtime_planners`の枠の数×窓の長さで割った占有率を添える。人が開いたplanner（`session_open.planner`）は別に添える。
- 導入の後のSpikeの結果待ちは、plannerが枠を持って待つ形ではなく、再計画の依頼（ADR-t1487-1決定5）が開いている時間として現れる。`dagq stats`の`planner_routes`の`request_settle`（依頼の開きから決着まで）を並べて読む。

## 2. 前例の選び方と基準値

### 選び方

`dagq search spike --kind task --limit 200`（149件）の題と、task 1499の説明が挙げた前例（task 553・812・1061、goal 65の961・968・969、goal 73・57）から、次の3つを満たすものを選んだ。

1. 計画を左右する未確認の前提を、実験・試作・測定で確かめるtaskである（題か説明に「spike」「確かめる」「前提」がある）。
2. その結果を読んで、同じgoalの実装（ADRを含む）が依存したか、再計画された。
3. Sの最初のsubmitが境界（2026-10-13T16:53:31Z）より前。

| 前例 | goal | S | 選んだ根拠 |
| --- | --- | --- | --- |
| P1 | 51（S は goal なし） | 553 | `spike:`の名のtask。予測の担い手を確かめ、結果を読んで人のplannerがgoal 51を作り、follow_upのtask 574が着地した |
| P2 | 57 | 812・864 | 使い捨てのrepositoryで非対話のworkerの未測定の点を確かめ、ADR（813）と実装が依存した。864は実Claude Codeの未確認の3点の追加の確認 |
| P3 | 73 | 1061 | Codexのgoal review jobが読み取りだけのsandboxで動くかを確かめ、ADR（1063）と実装が依存した |
| P4 | 65 | 961・968・969 | goalのacceptanceが「spikeで効果の見込みを確かめ、進めると決めた場合」の実装を求めた |
| P5 | 77 | 1180（後に3336〜3339） | Codexの対話のsessionで要るものを測り、ADR（1181）が依存した。1180のclaimは境界の後（2026-10-23）で、3336〜3339は区分の導入の後のSpike。基準値に入れず、境界の後の参考にする |
| P6 | 38 | 2170・2171・2280 | containerのruntimeの前提の確認（Spike）の手順書と結果を記録し、実装の前提にした。Sは境界の前、実装の19本のclaimのうち6本は境界の後。境界をまたぐ参考とする |

選ばなかった候補と理由: 932・1034・1281（負荷の山の出どころの調査で、計画の前提ではない）、945・1924（着地の後の分類・閾値の決定で、Spikeの後に実装が依存する形でない）、1117・1026（着地の後の効果の測定）、1476（取り消された確認）、review agentのevalのSpike（goal 125。queueの外で流した）。境界の後のSpike（goal 184の3171、goal 224の3440・3462、goal 77の3336〜3339、goal 139の3196）は導入の後の側の標本で、3章の窓で読む。

### (a) 総時間

| 前例 | Sの最初のsubmit | Sの最初の着地 | Iの数（Sと同時・後から） | 再計画の間 | 計画の総時間 | 除いた値 | 人の答え待ち | goalの総時間（除いた値） |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| P1 | 2026-09-26T09:29Z | 09-26T12:07Z | 6（0・6） | 0.1時間 | 48.6時間 | 48.5時間 | 0.0時間 | open（goal 51は未close） |
| P2 | 09-27T13:23Z | 09-27T16:18Z | 10（8・2） | 2.5時間 | 61.6時間 | 52.5時間 | 9.0時間 | 215.5時間（147.0時間） |
| P3 | 09-28T23:53Z | 09-29T01:29Z | 7（6・1） | 0.0時間 | 18.0時間 | 16.9時間 | 1.1時間 | 142.5時間（101.8時間） |
| P4 | 10-01T23:28Z | 10-02T07:47Z | 0 | — | open（Spikeの着地の後、実装も中止も決まらない） | — | 1.3時間 | open |
| P6（またぐ） | 10-08T12:28Z | 10-11T09:59Z | 16（11・5） | 2.9時間 | 376.7時間 | 343.3時間 | 33.4時間 | 376.7時間（341.7時間） |
| P5（境界の後の参考） | 09-29T23:55Z | 10-24T00:41Z | 1（1・0） | 0.0時間 | 593.5時間 | 587.3時間 | 6.2時間 | open |

- 基準値は、着地まで行った純粋な前例P1〜P3の中央で、計画の総時間48.6時間・除いた値48.5時間。P4の型（Spikeの着地の後に計画がopenのまま残るもの）は、総時間でなく「Spikeの着地から14日のうちに次の手（実装のsubmit・追加のSpike・中止）が決まらない計画の数」として数え、導入の前は純粋な前例P1〜P4のうち1件（P4。P1のgoalのopenは計画の実装が着地した後なので数えない）。
- 導入の前の計画は、P2・P3のようにSpikeとIを同じproposalでsubmitし、Iを依存だけで待たせた（ADR-t1487-1決定6が禁じる形）。再計画の間が0に近いのはこのためで、結果を読んでから実装を決める導入の後の流れでは、再計画の間が延びるのが正しい。比べるのは総時間で、再計画の間だけで悪化と判じない。
- P5の1180は2026-09-29にsubmitされ、claimは2026-10-23（待ち245.3時間）だった。境界の後の待ちで、基準値には入れない。

### (b) 前提の誤りによる手戻り

| 前例 | S+Iのplan reviewの段（前提・他） | S+Iのrunの段（前提・他・不明） | Fのplan reviewの段（前提・他） | Fのrunの段（前提・他・不明） |
| --- | --- | --- | --- | --- |
| P1 | 0・0 | 0・1・0 | 0・1 | 0・5・0 |
| P2 | 0・1 | 0・2・1 | 1・1 | 1・13・0 |
| P3 | 1・1 | 1・1・0 | 0・1 | 0・3・1 |
| P4 | 1・0 | 0・5・0 | — | — |
| P6（またぐ） | 3・12 | 1・34・0 | 0・2 | 1・5・0 |
| P5（境界の後の参考） | 3・1 | 1・7・0 | 0・7 | 0・5・0 |

- 純粋な前例P1〜P4のS+Iで、前提の誤りはplan reviewの段で2件、runの段で1件（P3のtask 1067: 固定バイナリが先行のtaskを含む前提がclaimの時点で成り立たなかった）。runの段の手戻りの候補は11件のうち前提の誤り1件・不明1件（P2のtask 816の問い。本文もtopicも記録に無い）。
- P2のFのrunの段の1件はtask 1174（`nux`は対話のTUIだけが書くという前提の違い）、P6のIのrunの段の1件はtask 2179（`acceptance_infeasible`）、Fの1件はtask 2423のworkerの質問（`acceptance_infeasible`・`out_of_scope_change`）。
- 読みは全てscriptの`JUDGED`にあり、各候補の本文はscriptの出力の`rework[].text`にある。

### (c) 区分ごとのclaim待ち

| 母集団 | 区分 | 件数 | 中央 | p90 |
| --- | --- | --- | --- | --- |
| 前例（P1〜P4・P6の境界の前のclaim） | spike（手で分けたS） | 9 | 2.9時間 | 68.3時間 |
| 前例（P1〜P3） | implementation | 23 | 0.9時間 | 14.0時間 |
| 前例（P1〜P3・P6の境界の前のclaim） | implementation | 34 | 1.15時間 | 14.0時間 |
| 本番全体（`ready_at`のあるclaim、2026-10-08T21:06Z〜境界） | implementation | 121 | 2.5時間 | 47.5時間 |

- `ready_at`がclaimに載り始めたのはevent 141488（2026-10-08T21:06:32Z）で、それより前の本番全体のclaimの待ちは記録から直接は出ない。前例のそれより前のclaimは1章の読み方で補った。
- P6の2170（68.3時間）と実装の2172（68.1時間）は記録された`ready_at`による値で、goal 38がdraftから開く（2026-10-11T07:35Z）までの約67時間を含む。前例の(c)の中央はこの2本で動かないが、p90はこの型で決まる。
- scriptの`--population`の値は、同じ窓の`dagq stats --since … --until … --full`の`claim_waits`と一致した（境界の後の窓で、spike 10件の中央5,990秒・implementation 580件の中央17,795秒）。

### (d) plannerの枠の占有

| 母集団 | session | 開いていた時間 | 働いていた時間 | 開いたまま働いていない時間 | 占有率（2枠） |
| --- | --- | --- | --- | --- | --- |
| 本番全体のruntimeのplanner（境界の前の14日） | 1,298 | 173.1時間 | 34.9時間 | 138.3時間 | 25.8% |
| うちheadless | 962 | 106.3時間 | 26.5時間 | 79.8時間 | — |
| うちinteractive | 336 | 66.8時間 | 8.3時間 | 58.4時間 | — |
| 人が開いたplanner（同じ14日） | 24 | 45.3時間 | 12.2時間 | 33.1時間 | — |

| 前例 | 計画に紐づくruntimeのplanner | 開いていた時間 | 働いていた時間 | 差 |
| --- | --- | --- | --- | --- |
| P1 | 0 | 0 | 0 | 0 |
| P2 | 1 | 0.0時間 | 0.0時間 | 0.0時間 |
| P3 | 4 | 0.1時間 | 0.1時間 | 0.0時間 |
| P4 | 1 | 0.0時間 | 0.0時間 | 0.0時間 |
| P6（またぐ） | 26 | 1.1時間 | 0.7時間 | 0.4時間 |

- 前例の計画を立て、Spikeの結果を読んで再計画したのは、goalに紐づかない人のplannerか人自身だった。計画ごとの「結果待ちで開いたままの時間」は導入の前の記録から数えられないので、本番全体の「開いたまま働いていない時間」を基準値にし、計画ごとには再計画の間（(a)）とP4の型の件数で代える。
- 同じ14日の`planner_routes.headless.request_settle`（計画の依頼の決着まで）は101件・中央149秒・p90 430秒。

## 3. 比較の条件

### 窓

- 始まり: 境界の印（event 184541、2026-10-13T16:53:31Z）。導入の後の側のSpikeは、Sの最初のsubmitがこれより後の計画とする（境界をまたぐP5・P6の型は両方の側に入れない）。
- 長さと件数の下限: 印から28日以上、かつ(a)は計画がSpikeの起点で着地まで行ったものが5件以上、(c)はspikeのclaimが20件以上。足りなければ判定を出さず、件数を書いて延ばす。印から13日の時点（2026-10-26）でspikeのclaimは10件。
- 比べる相手: (a)(b)は2章の前例P1〜P4、(c)(d)は本番全体の境界の前の値（(c)は`ready_at`のある121件、(d)は14日）。(c)(d)の導入の後は、印から同じ長さの窓を`dagq stats --since 184541 --until <終わり> --full`の`claim_waits`と`dagq kpi --since 184541 --until <終わり>`で読む。

### 層

- `dagq kpi --area runtime`と`--change`（feature・fix・measure・docs）で分けて、(c)の実装の待ちと(d)が全体の変化と同じ向きかを見る。Spikeが実装の待ちを延ばしたかは、`--area runtime`の実装の待ちで読む（docs・configは軽い枠で別に流れる）。
- Spikeのtaskは`change`と独立なので、(c)のspikeはchangeで分けない。

### 交絡

| 交絡 | 読み方 |
| --- | --- |
| `parallel`（境界の前後とも3） | `dagq marks`の`derived:parallel`が窓の中で変われば、その前後で分けて読む |
| hostのload・disk（claimの控え） | `dagq stats`の`claim_deferrals`・`claim_holds`と`kpi`の`load_per_core`を並べ、diskの`queue_hold`の時間を(c)の待ちから除いた値も出す |
| 同時期の他の変更 | 境界の後にgoal 96の外で入ったもの: containerのruntime（goal 38、2026-10-14〜24）、review の回数の上限（goal 199）、fresh_session（2026-10-18〜19）、toolchain 1.99.1（2026-10-25）。`dagq marks --since 184541`の印を窓に並べ、(a)の各計画の区間に重なる印を表に書く |
| 計画の大きさ | (a)はIの数で割った「Iの1本あたりの総時間」も並べる（P1〜P3は6〜10本） |
| 人の答え待ち | (a)は除いた値で判定する |

### 判定の目安

- **(a) 総時間**: 導入の後の計画の総時間（人の答え待ちを除く）の中央が基準値48.5時間の1.5倍（約73時間）を超えたら悪化、同じかそれ未満なら許容とする。導入の後は結果を読んでから実装をsubmitするので、再計画の間が延びた分（再計画の依頼の`request_settle`とplan review）は総時間に含めて受け入れる。
- **P4の型**: Spikeの着地から14日のうちに次の手が決まらない計画が導入の後に1件でもあれば悪化（goal 96の再計画の依頼の永続化が効いていない）。0件なら改善。
- **(b) 手戻り**: S+Iのrunの段の前提の誤りを計画1件あたりで比べ、基準（純粋な前例4件で1件、0.25件/計画）を超えなければ許容、0なら改善。plan reviewの段の前提の誤りは増えてよい（runの前に止まるのでworkerの時間を使わない）。
- **(c) claim待ち**: spikeの待ちの中央がimplementationの待ちの中央の2倍を超えるか、どちらかの区分のp90が基準（implementation 47.5時間）の1.5倍を超えたら、区分の上限かagingの設定（`dagq.toml`の`max_spikes`・`class_aging`）を見直す候補にする。implementationの待ちの中央が基準2.5時間から1.5倍を超えて延び、`--area runtime`でも同じなら、Spikeが実装の枠を埋めた疑いとして`claim_deferrals`の`spike_limit`・`investigation_limit`の件数と並べる。
- **(d) plannerの枠**: runtimeのplannerの開いたまま働いていない時間の窓あたりの割合（基準138.3 / 173.1 = 80%）が上がらず、Spike起点の再計画の依頼の決着のp90が1時間以下なら許容。Spikeの結果待ちでplannerのsessionが開いたままになる例（Spikeをsubmitしたplannerのsessionが、Spikeの着地まで閉じない）が1件でもあれば悪化。

## 4. 再現の方法

scriptは[spike-plan-baseline/](spike-plan-baseline/)にある。`fetch.py`は`dagq events --all --full`を`--after`と`--limit`でページを分けて読み、`$TMPDIR`の下のcacheにgoalとtaskごとに残す。`measure.py`は1章の定義で数え、出力（JSON）は区分がビューの1回きりの測定の材料で、同じコマンドで作り直せる（cacheを消せば本番のqueueを読み直す）。前例の出力は[baseline.json](spike-plan-baseline/baseline.json)、境界の前の(c)の本番全体は[claim-waits-before.json](spike-plan-baseline/claim-waits-before.json)。

2026-10-26T12:30Z〜13:10Zに、このrepositoryのworktreeで次を流した。

```sh
python3 -I docs/plans/spike-plan-baseline/measure.py --cache "$TMPDIR/cache" --json docs/plans/spike-plan-baseline/baseline.json
python3 -I docs/plans/spike-plan-baseline/measure.py --cache "$TMPDIR/cache" --population 2026-09-26 2026-10-13T16:53:31Z --json docs/plans/spike-plan-baseline/claim-waits-before.json
python3 -I docs/plans/spike-plan-baseline/measure.py --cache "$TMPDIR/cache" --population 2026-10-13T16:53:31Z 2026-10-26T12:00:00Z
dagq stats --since 2026-10-13T16:53:31Z --until 2026-10-26T12:00:00Z --full   # claim_waits の突き合わせ
dagq kpi --since 2026-09-29T16:53:31Z --until 2026-10-13T16:53:31Z            # (d) の本番全体（2 分あまりかかる）
dagq marks                                                                    # 境界の印 184541
dagq search spike --kind task --limit 200                                      # 前例の候補
```

- 境界: `dagq events --all --kind supervisor_config_changed --since 2026-10-13 --until 2026-10-14 --full`（event 184490）と`dagq marks`の`mark_recorded` 184541。
- 導入の後の評価は、`measure.py`の`PRECEDENTS`に導入の後の計画（goalと`execution_class`が`spike`のtask）を足して同じ定義で数え、`--population`を3章の窓で流す。`JUDGED`の読みは導入の後の候補にも1行ずつ足す。
