---
id: adr-t1487-1
type: adr
title: Spikeを変更の種類と独立したtaskの実行区分にし、結果を根拠付きの判定としてreceiptに持たせ、着地でruntimeが再計画の依頼を冪等に残し、調査中の計画の数と1つのgoalのSpikeの周期の数に上限を置く（ADR-t1394-1決定2・3・4・5・6をamends）
status: accepted
created: 2026-10-04
updated: 2026-10-04
accepted_on: 2026-10-04
amends:
  - adr-t1394-1 decision 2
  - adr-t1394-1 decision 3
  - adr-t1394-1 decision 4
  - adr-t1394-1 decision 5
  - adr-t1394-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - supervisor
related:
  - adr-t980-1
  - adr-t808-1
  - adr-t1394-1
  - adr-t451-1
  - adr-t1484-1
  - adr-t1487-2
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-draft-planners
---

# ADR-t1487-1: Spikeを変更の種類と独立したtaskの実行区分にし、結果を根拠付きの判定としてreceiptに持たせ、着地でruntimeが再計画の依頼を冪等に残し、調査中の計画の数と1つのgoalのSpikeの周期の数に上限を置く（ADR-t1394-1決定2・3・4・5・6をamends）

## Context

計画を左右する未確認の前提（Spike: 実験・試作・build・測定で確かめる問い）を、plannerは事実として扱うか、spikeを名乗るtask（553・812・1061など）を手順の約束だけで回している（goal 96）。runtimeはSpikeを区別しないので、Spikeと実装が枠を奪い合い、多くのgoalが同時に調査を出して膨らみ、結果を読んで再計画する仕事はplannerがsessionを開いて待つか人が思い出すかに頼り、再起動と引き継ぎで失われうる。本実装をSpikeの完了依存だけでreadyにすると、根拠を読まずに自動で始まる。Spike専用のworkerやjobのengineは作らず、既存のtask・run・receipt・runtimeのplanner・計画の依頼（ADR-t1394-1）を使う。

## Decision

1. **taskに実行区分を1つ足す。** 値は`implementation`（既定）と`spike`の2つで、runtimeが値の集合と意味を持つ（同時数・調査の上限・再計画の依頼を機械的に制御するため）。変更の種類（change、[ADR-t980-1](2026-09-29-t980-1-classify-runs-by-declared-change-and-diff-derived-area.md)）とは独立で、どのchangeとも組める。区分は優先度と同じく未着手のあいだだけ変えられ、claimの後は変えない。
2. **Spikeのtaskの形。** description（かacceptance）に、問い・後続の計画への影響・調査の方法・上限（時間・回数など）・証拠の所在・結果ごとの判断を持つ。acceptanceは「望む技術が使えた」ではなく、成立・不成立・未解決のどれかを根拠付きで判定することで、否定的な結果も調査の成功とする。形の検査はplan reviewの確認点で、runtimeは中身を検査しない。
3. **結果はreceiptの必須の欄に持たせる。** Spikeの区分のrunのreceiptは、判定（成立・不成立・未解決）・根拠・証拠の所在・証拠の条件（対象のcommit・toolの版・providerと経路・環境）を持ち、validatingはSpikeの区分のrunでだけ欠けを`needs_session`にする（実装のrunの検査は変えない）。権限・headless・providerの前提は、実際のworkerの経路で確かめた証拠があるときだけ成立とする（planner・subagent・人のterminalで確かめたものは成立の根拠にしない）。
4. **timeboxはagentへの指示にし、runtimeは新しい強制を足さない。** 上限はSpikeのdescriptionの上限の項目に値とともに「agentへの指示」として書き、receiptの結果に実際に使った時間・回数を書いて超過を見えるようにする。止める力は既存の上限（turnの沈黙と時間の上限、runの時間切れ）だけに任せる。
5. **Spikeの着地で再計画の依頼を冪等に残す。** Spikeのtaskが`completed`になったら、runtimeはADR-t1394-1の計画の依頼を、起点がSpikeであることとそのtaskを持つ形で、Spikeのtaskごとに1件だけ（taskを一意の鍵に）記録する。記録は着地（`completed`への遷移）と同じトランザクションで行い、加えてsupervisorのpassが「依頼の無い`completed`のSpike」を照合して補う（古いバイナリの着地・手でのcomplete・トランザクションの失敗を拾う）。一意の鍵があるので、再起動・execの引き継ぎ・2つのsupervisor・重複配送でも欠けも二重もない。依頼は着地の時点で永続しているので、workerの枠の解放とplannerの起動の順に依らず失われない。依頼のplannerはruntimeのplannerの同時の数の上限の枠でADR-t1394-1決定4のとおり立つ（二重起動の防止と回数の上限も同じ）。同じgoalのSpike起点の依頼のplannerは同時に1つだけ立て、後の依頼は前のplannerが終わってから立てる（後のplannerは前の結末も読み、同じgoalを並行して再計画しない）。Spikeをsubmitしたplannerは結果を待たずに終わる。workerはplannerを待たず、plannerはworkerを待たないので、互いの枠を持って待たない。`canceled`になったSpikeには依頼を作らない（取り消したplannerか人が手当てを決める）。
6. **再計画の判断。** 依頼のplannerは毎回、実装へ進む・追加の問いと上限を持つSpike・代替案・範囲の縮小・中止のどれかを決め、ADR-t1394-1決定6の結末（submitで`proposed`、中止は理由付きの`declined`）で終える。実装のtaskは改めてsubmitしてplan reviewを通る。本実装を最初からreadyにしてSpikeの完了依存だけで起動しない（候補の案はdraftで持ってよい）。出口は全ての不明点をなくすことではなく、残る不確実性を実装中に扱える状態である。
7. **調査中の計画の同時の数に上限を置く。** 調査中の計画は、Spikeのtaskが1つでもclaimされたgoalで、未完了のSpike（`draft`を除く）か`open`のSpike起点の依頼が残るあいだのもの。goalの無いSpikeはclaimから完了（依頼の結末）までそれ自体を1件と数える。ただし、残るものが全て人の答えだけを待つ（Spikeのrunの人への問い、依頼のplannerの`planner_question`、追加のSpikeのproposalの人の承認）調査中の計画は、ADR-t1484-1と同じ考えで同じ猶予を過ぎたら数えない（人の不在で全ての新しい調査を止めない）。上限は`[supervisor]`の設定で、無ければ上限なし（今の振る舞い）。上限に達したら、submitは拒まず、まだ調査中でないgoal（とgoalの無いSpike）のSpikeのclaimを控えて控えの記録を残し、上限を下回ったpassで再開する。例外として、調査中の計画の未完了のSpikeが（推移的に）待っているSpikeは上限に依らずclaimできる（控えると待つ側が枠を返さず詰まるため。上限を一時的に超えうる）。plannerの枠は占有しない（待つのはreadyのtaskで、plannerではない）。

   | 場面 | 枠 |
   | --- | --- |
   | goalの最初のSpikeがclaimされた | 取る（上限内のときだけclaimできる） |
   | 同じgoalの別のSpike・追加のSpikeがready・claim | 保持（同じgoalは数え直さない） |
   | Spikeが完了し依頼が`open`（plannerを待つ・作業中） | 保持 |
   | 残るものが全て人の答えだけを待ち、猶予を過ぎた | 解放（ADR-t1484-1と同じく人だけの待ちで流れを止めない）。答えの後は上限を超えても数え直し、新しい調査の開始だけを控える |
   | 依頼が`proposed`で、追加のSpikeがsubmit〜readyにある | 保持 |
   | 実装へ進む・代替案・縮小で、未完了のSpikeも`open`の依頼も無い | 解放（周期の終わり） |
   | 中止（`declined`）・`exhausted`・全てのSpikeの`canceled` | 解放 |
   | 未解決の判定 | 他と同じ（延長しない。決定8） |

8. **1つのgoalのSpikeの周期の数に上限を置く。** 周期は、そのgoalが調査中の計画になった回数（決定7の取るから解放までを1回。同時に走った複数のSpikeは1回）で数え、上限は`dagq.toml`の設定にする。上限は追加のSpikeのsubmitで判定する: 上限に達したgoalの追加のSpikeは、runtimeのplannerのsubmitでは通らず、範囲の変更として人の判断（`planner_question`）か人のsubmitを経る。上限の前にsubmitしたSpikeの完了は、上限に達していても再計画の依頼を残す。追加のSpikeはgoalに属させ、goalの無いSpikeの再計画が追加の調査を出すときはgoalを作るか既存のgoalに付ける。未解決を理由にruntimeが自動でSpikeを延長・連鎖することはない。follow_upの深さの上限（[ADR-t808-1](2026-09-28-t808-1-runtime-planners-submit-follow-ups-up-to-depth-two.md)）とは別の柵で、Spikeの次の手は結果（決定3）に書いてfollow_upにしない。Spikeのreceiptのfollow_upは今までどおりの経路（深さの数え方も同じ）で、依頼のplannerのpromptにも載せるので、同じ手当てを二重に計画しない。
9. **責務の分け方。** plannerの短い読み取りの調査（既存のコード・記録・公式文書を短く読む）の範囲と、Spikeに移す判断・登録の形・再計画の手順は共通のpluginの手順とruntimeのpromptが持つ。repository固有の証拠と重要性の基準・verifyは開発文書と`dagq.toml`が持つ。runtimeは区分・再計画の依頼の永続・区分と調査の上限・汎用のclaimの制御だけを持つ。plannerのsubagentに重い実験（build・測定・試作）をさせずworkerの枠を迂回しない。

ADR-t1394-1決定2の依頼は人の言葉を持つ記録だったが、Spike起点の依頼は人の言葉の代わりにSpikeのtaskとそのreceiptの参照を持ち、記録するactorはruntimeになる（CLIで依頼を記録できるactorは変えない。決定3）。決定4の「`open`の依頼1件ごとにplannerを立てる」は、Spike起点の依頼では同じgoalのものを同時に1つに絞る（決定5）。決定5の初期promptは、Spike起点の依頼では人の言葉の代わりにSpikeの結果（判定・根拠・証拠の条件）・follow_up・goalと、決定6の5つの選択肢を載せる。決定6の結末の知らせは、依頼した人が居ないので、Spike起点の`proposed`と`declined`（中止）はinboxへの知らせだけにし、attentionにするのは`exhausted`だけにする。表・欄・eventのkind・設定の名前と既定値はADR-t598-1決定2・3のとおり`docs/design/`に書く。

## Alternatives

- **change=`measure`で判別する**: `measure`は測定の文書化の着地にも使い、Spikeでない`measure`が多い。changeの値の集合はrepositoryが`dagq.toml`で決める集計の軸で（ADR-t980-1決定4）、制御の軸に流用するとrepositoryが値を変えたときに制御が壊れる。
- **区分を持たず手順だけで回す**: 手順は同時の数とclaimの公平性を保証できず、再計画の依頼の永続もできない（今の問題そのもの）。
- **結果を文書の決まった節に書かせる**: 依頼のplannerが文書を開かないと結果を読めず、validatingが欠けを機械的に見つけられず、証拠の条件を照合しにくい。
- **timeboxをruntimeが強制する**: 適切な上限は問いごとに違い、既存のstallと時間の上限で暴走は止まる。強制を足すと止めどころの判断をruntimeに持ち込む。
- **Spikeの完了で、submitしたplannerに結果を返す**: plannerがsessionを保持して待つことになり、plannerの枠を占有し、再起動で失われる。
- **依頼の記録をpassの照合だけにする**: 照合までの遅れと、照合の前の再起動での取りこぼしの説明が要る。着地と同じトランザクションを主にし、照合は補いにする。
- **調査中の計画を上限で拒む（submitを拒む）**: plannerが待つか作り直すことになり、plannerの枠を使う。readyで待たせればplannerは終われる。
- **readyのSpikeを持つgoalを調査中に数える**: claimされていない計画が枠を取り、始まっていない調査が上限を埋める。
- **人の答えを待つあいだも枠を保持する**: 人の不在で全ての新しい調査が止まる（ADR-t1484-1と同じ理由）。
- **未解決なら自動で追加のSpikeを出す**: 根拠を見ない延長が連鎖し、goal 96の制約に反する。

## Consequences

- Spikeの結果から再計画のplannerの起動までがsessionの保持なしでつながる。plannerはSpikeをsubmitしたら終わる。
- taskの欄・receiptの欄・依頼の起点・`[supervisor]`の設定が増える（機械的な制御に要る最小限）。区分を知らない固定バイナリは新しい設定を読めないので、`dagq.toml`に値を置くのは固定バイナリが読めるようになってから。
- 区分ごとの同時数と公平なclaimは[ADR-t1487-2](2026-10-04-t1487-2-spikes-share-the-worker-slots-under-a-cap-with-cross-class-aging.md)が決める。
