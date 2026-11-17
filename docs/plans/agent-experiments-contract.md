---
id: plan-agent-experiments-contract
type: plan
title: Agent比較実験の実装前の追加契約
status: active
created: 2026-11-17
related:
  - adr-t5120-1
  - adr-t5120-2
  - adr-t5121-1
  - adr-t5121-2
  - adr-t5121-3
  - design-agent-experiment-evidence
  - design-agent-experiments
  - plan-agent-experiments-brief
  - plan-agent-experiments-survey
---

# Agent比較実験の実装前の追加契約

## 位置づけと正本の移し方

[責務境界のADR](../adr/2026-11-17-t5120-1-agent-experiment-responsibility-boundary.md)と[役割の割当のADR](../adr/2026-11-17-t5120-2-role-assignment-extension-contract.md)の具体化。
実装前の契約であり、現在のbinaryが以下のTOMLを読めるという説明ではない。
入力の原文は[brief](agent-experiments-brief.md)、実装根拠は[survey](agent-experiments-survey.md)、流れは[設計書](../design/agent-experiments.md)が持つ。
原文のYAMLは概念例で、実際の案は今の設定と同じTOMLの名前付きの表にする。

一般のA/Bの共通契約はtask 1692が担当する。
作成時点ではADR-t1692-1とその契約文書は存在せず、task contextに渡されたgoal 114の計画方針を前提にする。
実験の宣言の項目と置き場、群のhashの算法と符号化、版のhash、実験属性の名前、最初の割当記録からの復元、収集の終了と決着は**共通契約の文書が持つ**。
この文書はそれらのkey・型・既定・値を定義しない。
以下の「共通の実験」「共通の群」「共通の版」「共通の単位」はその契約への参照であり、同名の第二の宣言ではない。
共通契約が確定したら接続先を照合し、方針を変えず接続できなければ実装を止めて決定の変更を依頼する。

各節末の「正本の移管先」は、予定するコードの責務と現在の入口を示し、存在しない将来の型名を実装済みとして引用しない。
**各項目を実装するtaskは、同じ変更で値・key・型・既定・検査・式を定義のそばのdoc commentへ移す。**
その項目の値と表と例をここから消し、`実装済み: 正本は <コードの入口>`に書き換える。
入口は実在する設定を読む型・既定値・関数を名指し、参照だけで値を知れるようにする。
一部だけ実装した場合は未実装の項目を残し、未実装の契約を削らない。
全項目の移管後にこの文書をcompletedにする。
項目を使う実装の段はtask 5122の計画が書き足す。
Evidence・Measurement・書き出しの追加契約は後半の節が持つ。
割当の定義は前半、観測への参照と外部表現は後半を正本とし、同じ項目を二重に定義しない。

## 1. 名前付きの割当先

### TOMLの形

`dagq.toml`に追加する表の案は次のとおり。
名前はreviewの検査定義の名前ではなく起動条件の名前である。
以下のmodel名・時間は例で、runtimeの既定や推奨値ではない。

```toml
[agents.claude_work]
provider = "claude"
model = "opus"
effort = "medium"

[agents.codex_work]
provider = "codex"
effort = "high"

[agents.claude_review]
provider = "claude"
effort = "high"
execution = { timeout_seconds = 600 }
```

| 項目 | 型・既定 | 検査と意味 |
| --- | --- | --- |
| `agents` | 名前から表へのmap、省略は空 | 空なら新しい割当先を登録しない |
| `<name>` | 空でないASCIIの英小文字・数字・`_`・`-`の文字列 | 参照は完全一致、重複は拒否。予約の検査定義名との同一性は仮定しない |
| `provider` | 文字列、必須 | provider adapterが認めるproviderだけ。役割との対応も利用時に検査 |
| `model` | 任意の空でない文字列、省略可 | providerと役割の起動能力で検査。省略は通常の同providerの解決規則で補い、割当時に実効値を固定 |
| `effort` | 任意の文字列、省略可 | providerと役割が受ける値だけ。補完と固定はmodelと同じ |
| `execution` | 表、省略は空 | 実行条件だけ。環境・prompt・フラグ・指示の上書きを含めない |
| `execution.timeout_seconds` | 正の64bit整数、省略可 | 役割の通常の上限を短縮する場合だけ受理。上限や隔離を緩める指定は拒否 |

未知の表内key、空の名前、不正な型は設定全体の誤りとし、黙ってその候補を除いて重みを再正規化しない。
同一providerを持つ別名は許すが、同じ名前の変更はその名前を参照する新規割当の条件変更になる。
modelを明示できないadapterにはmodelを指定できない。
省略と「実モデルが観測できない」は別で、起動へ渡さない値はその事実を固定し、観測値は後から別に記録する。
実行設定を増やすときも許可リストを型のそばで拡張し、任意のCLI引数や権限の設定を通す口にしない。

**根拠:** providerとmodelの整合検査と既存の役割の起動を再利用し、agentカタログに認可や実行環境の第二の設定系を作らないため。
通常の補完を割当時に解決することで、省略した値だけがretry時の最新設定へ漏れるのを防ぐ。
**正本の移管先:** 設定を読む型（現在の入口は`src/infrastructure/run_env.rs`の`Config`）と、役割の起動解決（`src/domain/actor_model.rs`の`RoleModels`）のdoc comment。

## 2. 役割の割当のpayload

共通の実験の宣言に結び付く追加payloadだけを、`dagq.toml`の`agent_routing`の名前付きの表で受ける。
表の名前は共通の実験名の参照で、ここでは実験名を新しく定義しない。
共通の宣言そのものをこの表に入れない。
実験の宣言の置き場がどちらに決まっても、読み込み後に名前で結合する。

次の例の実験と群は、共通契約の文書が持つ宣言で別途定義されているものとする。
この例だけでは実験を開始できず、割合・仮説・終了条件等の第二の宣言でもない。

```toml
[agent_routing.worker_speed]
mode = "independent"
actor = "worker"

[agent_routing.worker_speed.variants.control]
worker = "claude_work"

[agent_routing.worker_speed.variants.treatment]
worker = "codex_work"

[agent_routing.work_review]
mode = "combination"

[agent_routing.work_review.variants.same]
worker = "claude_work"
review = "claude_review"

[agent_routing.work_review.variants.partial]
worker = "codex_work"
# reviewは通常の設定。ただしwork_reviewが予約する役割には含まれる。
```

この二つの実験はworkerを共に予約するため同時には有効にできない。
例の並置は両者の構文を示すだけである。

| 項目 | 型・既定 | 検査と意味 |
| --- | --- | --- |
| `agent_routing` | 実験名から表へのmap、省略は空 | 共通の宣言に存在しない名前は拒否。payloadだけで開始しない |
| `mode` | `independent`か`combination`、必須 | 解決と競合の意味を明示し、群数から推測しない |
| `actor` | 役割の文字列、Independentで必須 | Combinationでの指定は拒否 |
| `planning_unit` | `request`か`proposal`、計画側で必須 | 実行側では指定不可。共通のtask/goalの単位は重複してここに置かない |
| `variants` | 共通の群名から役割mapへのmap、必須 | 共通の群名の集合と完全一致させる。重みはここに置かない |
| `variants.<variant>.<actor>` | 割当先の名前の文字列、省略可 | 登録済みの割当先を参照。省略はその群で通常の設定を使う |

役割の綴りは`runtime_planner`・`plan_review`・`worker`・`review`。
人が開く廃止済みの`planner`や、goal review等は受け付けない。
Independentでは`actor`以外の役割を群に書けない。
通常の設定をcontrolとして比較できるよう空の群mapを許すが、全群が空なら何も比較しないので拒否する。
Combinationの全群の和集合には複数の役割を要する。
同じproviderの組も許すが、それは旧providerの試しを変更する指定ではない。

全群の和集合を予約する役割の集合とする。
共通契約の実験の有効性と収集状態を用いて、新規割当を受ける二つの実験が同じ役割を予約するなら設定を拒否する。
IndependentをCombinationより下に隠して同時有効にすることも、ゼロの重みで競合を回避することもしない。
既に固定された古い割当は新しい実験のpayloadと合成せず、固定した集合を復元する。

解決の順は、選んだCombinationの明示した役割、予約の重ならないIndependent、通常の役割設定。
Combinationの和集合にあるが選んだ群で省略された役割は通常の設定に解決する。
この役割をIndependentで補うことは競合になる。
同じ役割の優先度を数値で選ぶkeyは設けない。

**根拠:** 群に一部の役割だけを書ける形と、二つの実験が同じ役割を触れない約束を両立させるため。
重み・宣言の有効性は共通契約を参照し、二つの割合の設定を持たない。
**正本の移管先:** 追加payloadの設定型と、役割の集合を検査・解決するdomain関数のdoc comment（現在の起動入口は`RoleModels::review_plan`と`worker_model::choose`）。

## 3. 割当の単位と計画側の追加

| 役割の範囲 | 単位の参照または追加値 | 固定する対象 |
| --- | --- | --- |
| worker・review | 共通契約のtask（初期）またはgoal | taskでは全runとreviewの全attempt、goalでは共通契約の所属規則に従う |
| runtime planner単独 | `planning_unit = "request"` | 同じrequestから開くplannerと差し戻しで開き直すplanner |
| plan review単独 | `planning_unit = "proposal"` | 同じproposalのreviewの全attempt |
| plannerとplan reviewの組 | `planning_unit = "request"` | 同じrequestのplannerと、そのrequestに解決されたproposalのplan review |

計画側の追加単位は役割割当専用で、一般の値の上書きの宣言にrequest/proposalを追加するものではない。
planner単独のproposal単位、計画側と実行側の混在、plan review単独のrequest単位は初期には拒否する。
requestへの解決は既存のcreation originの規則を共有する。
plannerのrequest、無ければ開き直すproposalに結ばれたrequestを使い、複数なら最小のIDを選ぶ。
その解決した参照を最初の記録に固定し、後の関連付けの変更では引き直さない。
requestが無いplannerは実験外と理由を記録する。
requestの組を使うplan reviewでrequestを解決できない場合も実験外で、別のproposalの抽選に黙って振り替えない。

plan review単独の実験は、plannerも予約するrequestの組の実験と同時には使えない。
proposalが変われば単独実験の対象は変わるが、同じrequestの組はそのrequestのまま。
下流taskは計画側の割当参照を出どころとして持ち、taskの抽選の入力にはしない。
taskとrequestを同じ整数だからと同じ対象に扱わない。
requestが無い仕事を対象にする拡張や全役の一括Combinationは、固定境界と比較母集団を決める別の契約を要する。

**根拠:** retryで変わるrunよりtaskを選び、task以前の役割には既存の永続IDを用いるため。
plannerが生成するtaskの数でplannerの標本数を水増しせず、出どころを使った分析を可能にする。
**正本の移管先:** 計画対象を解決するdomain関数とplanner・plan reviewの割当を確保するstoreのdoc comment（現在の入口は`src/application/planner.rs`と`PlanningState::start_plan_review`）。

## 4. 抽選への入力・版・seedと多群の式

共通の群の抽選を別のhashで作り直さない。
役割側が渡す割当のkeyは、実行側なら共通契約のtask/goalの対象、計画側なら型付きの`request(ID)`または`proposal(ID)`。
型付きの対象の符号化、実験名との結合、digestから乱数相当の値への変換は**共通契約の文書が持つ**。
計画側は同じ共通の抽選の口に対象の種類を追加し、整数IDの衝突を避ける。

独立の`seed`設定は設けない。
実効seedは共通契約の実験名による抽選の名前空間であり、独立な抽選系列が要れば新しい実験名を宣言する。
版はseedにしない。
共通の版と群の固定を前提とし、宣言を編集しただけで割当済みの対象を振り直すために版をsaltへ足さない。
seed・版・対象のbyte列をこの文書で再定義しない。

役割payloadと参照されたagentの解決済みの内容は、共通の宣言内容の一部として版の計算へ渡す。
agentの名前だけを渡して、その実体の変更が版に反映されない形にはしない。
通常設定を使う群の補完済み条件も割当記録に残す。
共通契約のhashの範囲・正規化はその文書が持ち、この文書は別の「agentの版」を作らない。
同じ版・名前空間・対象なら同じ抽選を説明できるが、modelの実体更新・CLI・入力・外部状態・LLMの確率性まで再現できるという約束ではない。

### 多群の選択

共通契約から受ける群名と非負の重みの列を使う。
重みの宣言のkeyと型は**共通契約の文書が持つ**。
多群の計算には、その型を損失なく扱う整数または有理数を使い、浮動小数点の丸めで境界を変えない。
群名のUTF-8 byte列の昇順で列を整列する。
群を`v_1 ... v_n`、対応する重みを`w_i`、総和を`W = Σ w_i`とする。
`W > 0`を要し、共通の抽選が返す一様な値を`u ∈ [0,1)`とする。
選ぶ群は`u * W < Σ(j=1..i) w_j`を初めて満たす`v_i`。
境界が等しければ次の正の重みの群になり、重み0の群は選ばない。
共通の抽選が有限整数`h ∈ [0,M)`を返す場合は、同じ比較を`h * W < M * cumulative_i`の交差積で行う。
積と和は検査付きの拡張整数で計算し、表現不能なら設定エラーとする。
`M`やdigestの切り方はここでは決めない。

重み`1, 2, 1`なら確率は`1/4, 1/2, 1/4`で、`u = 1/4`は第2群、`u = 3/4`は第3群。
これは件数のノルマではなく、少数のtaskでこの比に揃う保証はない。
層の選択と対象外の判定は共通契約と互換の対象判定に従い、その後に選んだ群を記録する。

**根拠:** 二群専用の百分率比較を増やさず多群へ拡張し、記述順・丸め・並行claimで抽選が変わらないようにするため。
有限のhash値の離散化による差と少数標本の揺れはあり、確率を厳密な件数比とは説明しない。
**正本の移管先:** 共通の割当関数への役割対象のadapterと多群の選択関数のdoc comment（現在の二群の入口は`WorkerTrial::draw`）。

## 5. 割当の記録と起動の記録

以下は論理的な保存項目の追加契約で、eventのkind・wireのJSON・書き出しのschemaの定義ではない。
共通の実験名・版・群・最初の記録の識別は**共通契約の文書が持つ**。
その参照に次の役割固有の項目を結び付ける。

| 対象 | 残す項目 | 意味 |
| --- | --- | --- |
| 最初の役割割当 | 型付きの対象、解決したrequest/proposalの参照、計画側の割当参照 | 固定対象と下流の出どころを区別 |
| 最初の役割割当 | 方式、予約役割の集合、群の役割map、省略の印 | 部分指定と競合の根拠 |
| 最初の役割割当 | 役割ごとの名前付きの割当先と解決済みprovider・model・effort・実行条件 | 名前の実体が変わっても復元できる意図 |
| 最初の役割割当 | 通常設定に解決した役割とreviewのjob別の起動条件 | 省略の意味を最新設定で読み替えない |
| 抽選の説明 | 共通の抽選入力・版への参照、実効seedの参照、群の順序・重みと選んだ区間 | 算法の版と入力から選択を監査。共通の値の複製を正本にしない |
| 起動の試みごと | 割当への参照、役割、run/attempt/job/turn等の既存の実行参照 | 同じ割当から複数の起動が出る関係 |
| 起動の試みごと | 意図した割当先、介入・段上げ後かつfallback前の予定の起動条件、実際に渡した条件 | 意図・予定・実態を分離 |
| 起動の試みごと | 実際に観測したprovider・model・effort、観測不能の理由 | 要求値を実モデルの観測値と偽らない |
| 起動の試みごと | 起動しなかった理由、待ち・fallback・介入・段上げの理由と元の条件 | 使えなかったものを成功群から落とさない |

最初の記録と対象の確保は同じtransactionにし、同じ対象・実験の最初の割当を並行して二つ作れないようにする。
runやjobはその割当を参照し、再起動時に台帳の集計結果から割当を推測しない。
workerのclaimは開いた属性に共通契約の群と割当への参照を載せる。
planner・plan reviewはtaskのclaimを持たないので、自身の永続記録に割当参照を置き、下流の出どころと結ぶ。
reviewのjobはそのrunの役割割当を読み、job別の予定と実際を記録する。

**根拠:** 割当の正本と起動の実態を分け、同じ群の中のfallbackや欠測を分析で除いたり含めたりできるようにするため。
**正本の移管先:** 共通の最初の記録への役割拡張の型、storeの一意性の契約、起動記録の型のdoc comment（現在の入口は`claim_task_filtered`と`ActorLaunch`）。

## 6. 固定・fallback・reviewの解決

最初に固定した役割の割当はresume・revise・retry・supervisorの引き継ぎでも復元する。
重み・agent・通常設定の変更、宣言の停止・削除は既存の割当を書き換えない。
新規割当を止める境界と新しい版の開始は共通契約に従う。
既存の対象へ新しい実験を途中から重ねず、固定した割当の集合をその仕事の終了まで使う。
古い版を復元できない場合は、その欠損を記録して既存の復旧経路へ返し、最新の宣言で抽選し直さない。

fallbackはworker/jobそれぞれの既存の許可設定を読み、禁止なら待つ。
許可なら同じ割当のまま、その時点で利用可能な既存の経路を使う。
providerが変わるときは元provider専用のmodelを渡さず、切り替え先の通常の起動条件を解決してその試みに記録する。
fallbackの結果を新しい意図や群として保存しない。
復旧した後の新しい起動は元の意図から可用性を再評価するが、生きたsessionを実験のために強制終了しない。
削除されたagentの名前は保存した実体で復元する。
実体のprovider等がbinaryで使えなければ利用不能として扱い、同名の最新定義へ黙って置き換えない。

reviewの割当が明示される群では、全体とagent別jobの起動にその割当先を適用する。
検査の定義と信頼する規則は従来どおりで、名前付きのreview定義そのものを置き換えない。
reviewを省略する群では既存のagent別設定の混在を保ち、最初に解決したjob別の条件を固定する。
新たな検査定義によりjobが増えた場合は、固定した役割の条件から解決し、定義の版と新しいjobであることを記録する。
worker用の環境・prompt等の値の上書きはreviewへ渡さず、integrateにも役割の割当を渡さない。
段上げと人の介入は既存の決定の範囲で適用し、割当と実態の差として記録する。

**根拠:** 条件の固定は可用性の固定や検査定義の凍結ではないため。
review自身のagentの選択と、workerの値をreviewへ漏らすことを別の入口にする。
**正本の移管先:** 割当復元・fallback適用とreviewのjobの起動解決のdoc comment（現在の入口は`RoleModels::review_plan`・`job_start_route`・`WorkerSession::current`）。

## 7. 今の二つの試しからの接続

この表は移す方向と保つ意味の契約であり、現在の設定を編集する手順ではない。
今のkeyの正本は`WorkerTrial`と`ProviderTrial`のdoc comment。
共通の宣言に移す項目の新しい綴り・置き場は**共通契約の文書が持つ**。

| 現在の入力 | 接続先と保つ意味 |
| --- | --- |
| `[worker.trial].enabled` | 共通の宣言の新規割当の有効性へ。既存の群の無効化の扱いを変える段は別ADRを要する |
| `.name` | 共通の実験の識別への対応表。過去の名前と群を残し、新しい名前で過去の抽選を再実行しない |
| `.treatment_percent` | 共通のcontrol/treatmentの重みへ対応。割合の第二の入力を役割payloadに持たない |
| `.control_model`・`.control_effort` | Claudeの名前付き割当先を参照するIndependentのcontrol |
| `.treatment_model`・`.treatment_effort` | 同じくtreatment。旧段の制約と段上げは移行まで保つ |
| `.limited`・`.window` | 旧対象判定の互換adapterへ。予測の有無・mechanical・下位の範囲を勝手に全taskへ広げない |
| `[worker.provider_trial].enabled`・`.name`・`.treatment_percent` | 共通の宣言の有効性・識別・群の重みに対応 |
| `.treatment_worker`・`.treatment_review` | Combinationのtreatmentの二つの割当先 |
| providerの試しのcontrol | 通常設定を使う空の役割map。job別の起動条件を割当時に固定 |

過去の記録は旧試しの識別を保つ読取adapterで扱い、新しい版のAssignmentが当時存在したように捏造しない。
旧記録にない実効値・版・予定は不明と理由を出す。
移行後の新規対象にだけ、共通契約による最初の記録と役割payloadの解決を適用する。

二つの旧試しはworkerを共に触り、Aだけでmodelの試しを重ねる現在の振る舞いは、新模型の二つの同時有効な実験には直接写せない。
そのまま保つ間は旧経路で動かす。
新しい宣言へ移す段では、同じworkerを予約する実験を停止してから一つを有効にするか、一つのCombinationの群にまとめて比較条件を明示し直す。
旧Bのfallback先がClaudeでもmodelの試しの対象外であること、明示provider・Spike・headless外・hold解除の対象外も、移行で自動解除しない。
一般模型で同種providerの組を表現できることは、旧試しの異種という条件を取り消すものではない。

新しい仕組みが無いrepositoryでは旧二表の読み込みも挙動も変えない。
移行対応後も、同じ役割について旧試しの有効化と新payloadの有効化を併用する設定は拒否し、暗黙の二重抽選を防ぐ。
旧writerの記録も読めることを確かめ、固定binaryが新しい表を読めるようになる前に本番設定へ追加しない。
runnerのpull経路を含め、同じ割当・復元を通せない起動経路は実験外と理由を記録し、比較へ黙って混ぜない。

**根拠:** 既存の群の意味を保ち、長期には専用の二群のhashやkpi分岐を増やさず一つの宣言へ接続するため。
ADR-0079、ADR-t3957-1、ADR-t4005-1、ADR-t4118-1とその改訂はこの文書では変更しない。
変わる決定番号と内容はtask 5122の計画で挙げ、移す実装の段のADRがamendsを行う。
**正本の移管先:** 旧設定の読取・変換adapter、旧記録の読取adapterのdoc comment（現在の入口は`WorkerTrial`・`ProviderTrial`・`claim_task_filtered`）。

## 8. Evidenceの収集と由来

ここからは[境界のADR](../adr/2026-11-17-t5121-1-evidence-and-versioned-measurements.md)・[保存のADR](../adr/2026-11-17-t5121-2-bounded-evidence-storage-and-access.md)・[書き出しのADR](../adr/2026-11-17-t5121-3-versioned-experiment-export-and-modes.md)の具体化。
現行の記録の根拠は[surveyの§3〜6](agent-experiments-survey.md)である。
以下は追加する契約で、本文の収集や結合済みexportが現在動いているとは述べない。

### Dの項目と現行の記録・不足・追加する境界

表の「追加」は§5の割当・起動記録を再定義せず、その参照と観測の取得を足す責務を示す。
型名は現存する検索の入口で、将来の書き出し型の存在は仮定しない。
各行の根拠と移管先はその行の契約の一部であり、実装時には該当行だけを移管する。

| brief Dの箇条 | 今の記録と根拠（survey） | 欠けていること | 追加する場所・正本の移管先 |
| --- | --- | --- | --- |
| Experiment / Variant / Assignment / Run / Attempt / Actor InvocationのIDと関係 | §3: `TaskRun`・工程attempt・`SpanRef`・turn・`JobRef`。共通のInvocation集約は無い | 全役の起動の一意な参照、queueを跨いだ整数IDの名前空間、割当と成果物の対応 | §5の起動記録の参照をexecutorの開始前に採番しEventStoreに記録。起動とEvidence参照を表す型とexportの参照型のdoc comment |
| 実験設定・割当確率・抽選key・設定版・Task/Goal | §3・7: 旧trial名と群はclaim属性。新しい保存項目は本書§5、宣言と抽選の正本は共通契約 | 当時の共通記録と役割payloadへの不変の参照。goal所属の時点の区別 | 共通記録と§5への参照をEvidenceに付けるadapter。確率・key・設定版は既存の所有者の写しをexportへ射影し第二の宣言にしない。現在の入口`claim_facts`と属性eventの読取adapter |
| 意図と実際のProvider/Model/Effort、CLI・prompt・設定の版/hash | §1・4: `ActorLaunch`・`planned_provider`・`JobSession`・`ExecutionTokens`、起動実行ファイルの解決 | 意図・予定・実際の一貫した参照、実CLI版、組み立て済みpromptの安全な識別、価格版と設定版の分離 | 意図と予定は§5から参照。executorが実際に渡した条件と観測値・CLI版を記録。prompt組み立てとprovider adapterで安全なsnapshotを取得しEventStoreへ由来を記録。起動記録・snapshotの型のdoc comment |
| 入力要求・計画・review指摘と判定・修正ループ・成果物・diff・commit | §2〜4・6: request/proposal、receiptのeventへの取り込み、review verdict、`Snapshot`・`PlanInput::capture`、main履歴 | 当時の資料の本文の有限保持、各修正の入出力の同一性、消えたGit objectを理由付きで表すこと | 入力・review開始時と成果受領時のsnapshot取得adapter。runのbase・review対象head・着地commitを別参照にし、各attemptの前後の成果物を結ぶEvidence manifestのdoc comment |
| 開始・終了、キュー待ち、処理時間、token、費用 | §4: 工程event、台帳の共通の窓、Execution、`ProviderCost::usd`、資源sample | 測定定義の版・単位・窓、価格版と換算の印、開始前の待ちと起動時間の分離 | 時間・tokenは既存のfold/windowを利用し、必要な構造化観測だけ正本へ追加。費用は換算時の価格snapshotを結ぶ。測定定義とMeasurement射影の型、価格読取型のdoc comment |
| 個別test・受け入れ検証・失敗・timeout・cancel・fallback | §4・6: verification eventとlog、receipt、review program/agentの結果、turn/jobの終了 | test runnerごとの個別結果の正規化、起動しなかった試みと出力打切り、信頼する検証とworker申告の区別 | 検証executorの結果adapterが個別の構造化事実をEventStoreへ取り込み、元出力を有限保持。未対応runnerは明示の欠損。検証結果・起動終了・Evidence受領型のdoc comment |
| Gitの衝突・merge・着地と並列の観測 | §5: `Integrator`・`landing.rs`・`recheck.rs`・`advance_main`、main履歴event | 衝突と修正attempt・同時進行の対応、rebase前後と最終着地の区別 | 既存の制御側Git観測にEvidence参照を追加し、関連runと時点を保持。着地結果と衝突観測の型のdoc comment。Git所有権を別executorへ渡さない |
| 環境・base commit・道具の版 | §1・4・5: `environment_attached`・資源窓・claim属性・実行ファイル解決・`TaskRun` | 起動時の実効の道具/CLI版と環境snapshot、Benchmarkの初期条件の証明 | executorと環境adapterが許可した項目を収集。base/tree・CLI版・build・OS/arch・実行backend・並列度・負荷の窓を参照。環境snapshotとBenchmark入力型のdoc comment |

**根拠:** brief Dの全箇条を、既存の取得元・不足・追加責務に分解して、同じ観測を別々のwriterで捏造しないため。
**正本の移管先:** 表の各行に記した定義のそば。
表の対応項目を実装した変更で値と欄を消し、`実装済み: 正本は <コードの入口>`へ置き換える。

### 由来と欠測の追加規則

外部でのIDはqueueの永続識別と型付きのローカル参照の組とする。
新規queueには制御側が不透明なUUIDを一度発行して保存し、repositoryのpath・remote URL・task番号をqueue識別に流用しない。
backupの復元は同じ由来を保ち、独立運用するfork/importは新しい識別と元のqueueへの由来を持つ。
共通契約のIDを新しく採番し直さず、exportの名前空間で包むだけにする。
起動参照は開始前に固定し、配送の再送は同じ参照、別の起動の試みは別参照にする。
providerがsession名を後で返す場合も、制御側の起動参照は変えない。

runの無いplanner・plan reviewはrequest/proposal/job等の既存参照を使い、run/attemptを欠損理由付きにする。
同じtaskへのretryは別run、同じrunへの修正は別attemptや起動、計画の出どころは別の関係として結ぶ。
§5の起動記録を正本とし、ここでは別の固定単位や再抽選条件を設けない。
旧記録で対応不能な関係は`not_recorded`、意味の上で存在しない関係は`not_applicable`である。

**根拠:** survey §3のIDはqueue内で有効で、sessionやInvocationの意味は全役で同一でないため。
**正本の移管先:** queue identityの読取/初期化型、起動参照型、exportの型付き参照のdoc comment。

## 9. 版付きMeasurementの追加契約

測定定義は`name`・`version`・`unit`・`scope`・`method`・`inputs`を持つ。
`scope`は対象の種類と時間窓の端の意味、`method`は算法・丸め・分母・重複排除の定義、`inputs`は必要な構造化観測を指定する。
定義の`version`は正の整数で、意味・単位・窓・算法・入力・欠測や分母の扱いを変えたら上げる。
名前が同じでも版が違えば別の列として分析できるようにし、出力スキーマの版とは兼用しない。
共通契約が持つ数値の正本の置き場は再定義せず、その値への射影と説明を追加する。

| 最小の定義の案 | 単位・範囲 | 方法と入力の根拠 |
| --- | --- | --- |
| `run.elapsed` / 1 | `s`、runのclaimから既存の終端まで | 既存の工程のfold/window。push pendingを完了値にしない |
| `invocation.tokens.input` / 1 | `token`、起動一回 | Executionのinput。session区間と重複加算しない |
| `invocation.cost` / 1 | `USD`、起動一回 | provider報告の額、またはExecutionのtokenに固定した単価を掛ける。出所と価格snapshotを必須化 |

値は`definition_ref`・`subject_ref`・`window`・`input_cut`・`value`・`missing_reason`・`estimate`・`source_refs`・`coverage`を持つ。
`value`は有限の数またはnullで、nullなら`missing_reason`が必須、数なら同欄はnullとする。
単位は定義から解決でき、通貨も定義の一部である。
`estimate`は`kind`（`none`・`conversion`・`forecast`・`mixed`・`unknown`）、`basis_ref`、`missing_reason`を持つ。
換算/見込みなら基礎の単価や方法の安全なsnapshot参照が必須で、根拠が無ければ値をnullにする。
価格snapshotは適用したprovider/model、token区分ごとの単価、通貨、価格の出所、取得時点、版を持つ。
現在の設定から換算した旧データは、その換算時点の価格の版を使い、当時の価格や請求額とは呼ばない。
ADR-t4005-1決定8の単価の入力元を変えず、新しい価格設定のkeyを足さない。

`coverage`は`expected`・`observed`・`missing`を非負整数または理由付き不明で持ち、部分観測の小計か完全な値かを`complete`で示す。
不明な項目を含む完全な合計はnullとし、既知の小計を出す場合は別の定義と`complete=false`で示す。
推計が一つでも混ざる集計は`mixed`、出所を判定できなければ`unknown`として実額扱いしない。
異なる価格版の値はその参照を全て保ち、同じ通貨・範囲の算術だけを許す。

最小の欠損理由は`not_recorded`（収集前/旧writer）・`not_observed`（未到達/配送抜け）・`unsupported`（取得不能）・`not_applicable`（意味の上で対象外）・`expired`（保持切れ）・`size_limit`（量の制限）・`redacted`（マスキングで失われた）・`access_denied`（非開示）・`invalid_source`（不整合）とする。
`access_denied`を返すのは、既に認可された親の記録の中で一部だけ非開示の場合に限る。
対象自体に権限が無い問い合わせは、存在を確認できない拒否とし、識別子や件数を返さない。
実行中に値が未確定なら`not_observed`と到達点で示し、timeoutやcancelという実行結果自体はoutcomeに残す。

**根拠:** survey §4の既存の欠測・Execution・読み取り時の費用換算と、ADR-t1662-1〜3の台帳/送信の境界を保って、定義と由来を追加するため。
**正本の移管先:** 測定定義、値と被覆、価格snapshotの型のdoc comment。
数値を生成する関数のそばに式と入力を移し、書き出し型の説明から参照する。

## 10. 書き出しの最小スキーマ案

### framing・版・入力の固定

UTF-8のJSONLで1行1object、時刻はUTCのRFC 3339、byte数は圧縮前のbyte数、内容digestはマスキング後のbyte列のSHA-256とする。
digestは整合のためで、秘密の秘匿に使わない。
以下の`schema_version`は`1.0`を初期案とし、major.minorの文字列にする。
破壊的な変更（意味・必須性・型・参照/欠測の解釈）はmajorを上げ、意味を保つ任意欄/任意レコードの追加はminorを上げる。
同じ版の意味を上書きせず、未対応majorは拒否、minorの未知の任意情報は無視できる。
新しい必須レコードや新しい必須の列挙値はmajor変更であり、「任意の追加」と称して導入しない。
明示した対応版への出力adapterは、無い情報を欠損にするだけで、値や元の割当を作り直さない。

全行は`schema_version`・`export_id`・`record_id`・`kind`・`required`・`queue_id`・`mode`・`data`を持つ。
`record_id`は出力内で一意の文字列、`export_id`は1回の出力のUUID、`required`は未知kindを読み飛ばしてよいかのbooleanである。
初期案の以下のkindは全て`required=true`とし、必要な関係が無い種類は0件でよい。
未知の必須kindがあれば読み手は比較を開始せず拒む。
`queue_id`は§8の永続識別、`mode`は`operational`または`benchmark`であり、一つの出力で混在させない。
`replay`は将来の予約値で、初期のwriterと実行器は受け付けない。
型付き参照は`{type, id}`で、`id`は同じqueue内の既存IDの文字列表現である。
他queueへの由来だけは`queue_id`を参照へ明示し、無い場合は行のqueueを使う。

`manifest`で選択対象と窓・入力の到達点を固定し、先頭に一度出す。
manifestの`input_cut`はevent/step/nodeと台帳の入力到達点、台帳の版、観測時点を持ち、一貫した読み取りsnapshotで確定する。
他のレコードの`input_cut`はそのmanifestの`record_id`を参照する。
共通の窓がそのcutまで揃わなければ、待つか欠測を明示し、現在のStateStoreから穴埋めしない。
本文の利用可否と認可は読み出す時点でも検査するので、manifestは後の失効を無効化しない。
出力はmanifest、定義/条件、参照先から参照元の順を基本とし、循環の由来はIDで解決する。
最後の`end`以外の全行を改行を含むbyte列として順にhashし、`end`で件数とhashを照合する。
`end`が無い出力は未完了で、分析の完全な入力としない。

**根拠:** survey §7の人向けJSONとWireの差、§4の遅延入力、ADR-t5121-3の互換と部分出力の境界。
**正本の移管先:** export envelope・version・manifest・終端の型とserializerのdoc comment。

### レコードの種類とdataの欄

実験・版・群・割当の原ID、invocation行の起動ID、subjectとsource_refsは§8の型付き参照とする。
それ以外の本文・条件・割当・定義・outcomeから起動への参照は、同じ出力の`record_id`を指す文字列とする。
`contract_ref`だけは共通契約の形式と版を識別する文字列で、出力内のrecordを指さない。
参照不能な欄は同名の`*_missing_reason`を付けてnullにし、空の参照配列も理由を併記して「対象なし」と「対応が不明」を分ける。
定義の参照は欠けてよい値ではないため、定義の無いMeasurementは出さず出力をエラーにする。

| kind | dataの必須欄 | 意味・正本への接続 |
| --- | --- | --- |
| `manifest` | `selection`, `input_cut`, `created_at`, `mode_context`, `redaction_version` | 絞り込み、共通の窓の到達点、出力時点、比較条件と出力時の安全処理の版 |
| `condition` | `experiment_ref`, `experiment_version_ref`, `snapshot_ref`, `contract_ref` | 共通契約の実験の条件の不変の写しへの参照。snapshotは共通宣言と§5の役割payloadを所有者の形式/版のまま包む。重み・確率・抽選key・seed参照・設定版の意味をここで再定義しない |
| `assignment` | `assignment_ref`, `condition_ref`, `variant_ref`, `subject`, `assignment_record_ref`, `origin_refs` | 共通の最初の記録と§5の役割追加記録の写しを参照。履歴は最初の記録と各起動の利用の対応で表し、fallbackで群を更新しない |
| `invocation` | `invocation_ref`, `assignment_refs`, `role`, `subject`, `run`, `attempt`, `launch_record_ref`, `input_refs`, `output_refs` | §5の意図・予定・実際を含む起動記録の写しへの参照。実CLI・prompt/設定版の観測はそのEvidenceへ結ぶ。run/attemptは値と欠損理由のobject |
| `outcome` | `invocation_ref`, `result`, `reason`, `started_at`, `ended_at`, `evidence_refs` | `result`は`success`・`failed`・`timeout`・`canceled`・`not_started`・`running`・`unknown`。Git/検証等の内訳はEvidence参照。待ち・fallback・介入はlaunch記録のまま残す |
| `evidence` | `subject`, `category`, `source_refs`, `captured_at`, `content`, `redaction` | 構造化eventの参照または有限保持の本文。snapshot・transcript・diff・review・verification等の分類は内容の種類で、event kindの別名ではない |
| `measurement_definition` | §9の定義の全欄 | 単位・窓・方法の説明を一度出し、各値が参照する |
| `measurement` | §9の値の全欄 | 共通の台帳の窓からの射影。rawのEvidenceからexportが直接計算しない |
| `aggregate` | `definition_ref`, `selection`, `input_cut`, `operation`, `value`, `missing_reason`, `estimate`, `coverage`, `source_refs` | `operation`は定義が許す`count`・`sum`・`mean`・`rate`。層と分母をselection/定義に残し、順位・総合点を持たない |
| `end` | `record_count`, `sha256`, `complete` | 直前までの全行の件数とhash、`complete=true`だけが完了。件数はend自身を含まない |

`evidence.content`は`ref`・`digest`・`bytes`・`availability`・`missing_reason`・`expires_at`を持つ。
`availability`は`available`・`missing`・`withheld`で、後二者は理由が必須であり、ref/digest/bytesは開示可能なもの以外nullとする。
`ref`は認可付きの取得use caseが解釈する不透明なartifact識別子か、exportに添えるマスク済みのbundle内の識別子である。
絶対path・外部URL・署名URL・tokenは入れず、参照の寿命を`expires_at`で示す。
`redaction`は`version`・`applied`・`reason`を持ち、どの規則で何を除いたかは分類だけを残す。
`source_refs`はevent等の型付き参照であり、参照元の構造化した記録はproviderのsession参照とcollectorの信頼区分を持ち、agent申告と制御側観測を区別できる。

conditionのsnapshot、assignmentの原記録、invocationのlaunch記録も同じEvidenceの保持・マスキングを通す。
本文が欠けても実行/割当の参照自体を消さず、どの条件が確定できないかを示す。
共通契約の語彙はその契約の版を`contract_ref`に指定し、そのwireの型の責務をここへ移さない。

**根拠:** brief Eの条件・履歴・結果・値・成果物の結合と、survey §3〜6の既存参照を保つため。
**正本の移管先:** 各kindのexport型とEvidence参照型のdoc comment。
condition/assignment/launchの内側の意味は共通契約および§5の移管先を参照し、export型に複写しない。

### 通常運用・Benchmark・将来のReplay

`mode_context`は通常運用なら`{benchmark:null, replay:null}`とする。
Benchmarkなら`benchmark`に`series_id`・`case_id`・`repetition`・`input_snapshot_ref`・`initial_commit`・`initial_tree`・`environment_ref`・`deviations`を持ち、`replay`はnullとする。
`series_id`は制御した比較の組、`case_id`は入力の組、`repetition`は0以上の反復番号で、別群の対応を識別する。
同じcaseの比較は安全に固定した入力のdigestと初期commit/treeの一致を検査し、無い場合はBenchmarkとして受け付けない。
外部modelやhostの完全同一性は保証せず、起動条件・道具の版と逸脱を残す。
通常観測は後から同じ入力らしいと分かってもBenchmarkへ変更せず、Benchmarkの系列にも自動で混ぜない。

将来のReplay用には`mode_context.replay`と、invocationの`input_refs`に元の固定snapshot/成果物を参照する境界を予約する。
元のinvocationとReplayのinvocationは別IDとし、元の判定・期待の非開示と新しいreviewerの結果を分離する追加契約を実装前に要する。
初期案では`replay`の値は常にnullで、ReplayのCLI・scheduler・harness・移行は作らない。
evalとは`Snapshot` / `PlanInput::capture`と`RoleHarness`の取得・同一性検査・隔離・本番の起動を再利用する候補とし、evalの採用スコアをexportの順位に転用しない。

**根拠:** survey §5・6、ADR-t1728-1・ADR-t3948-1の固定入力・非開示・採用の境界とbrief E。
**正本の移管先:** exportのmode型、Benchmarkの入力型と検査のdoc comment。
Replayを足す段ではその型と認可の定義へ移し、初期の未対応を黙って解除しない。

### JSONの例（recordの形）

例のIDは架空で、schemaとMeasurementの二つの版は独立している。
これは単独レコードの例であり、完結したexportではない。

```json
{
  "schema_version": "1.0", "export_id": "10000000-0000-4000-8000-000000000001", "record_id": "m-cost",
  "kind": "measurement", "required": true, "queue_id": "20000000-0000-4000-8000-000000000001", "mode": "operational",
  "data": {
    "definition_ref": "def-cost", "subject_ref": {"type":"invocation","id":"launch-demo"},
    "window": {"start":"2026-11-17T00:00:00Z","end":"2026-11-17T00:01:00Z"},
    "input_cut": "manifest", "value": null, "missing_reason": "not_recorded",
    "estimate": {"kind":"unknown","basis_ref":null,"missing_reason":"not_recorded"},
    "source_refs": [{"type":"event","id":"event-demo"}],
    "coverage": {"expected":1,"observed":0,"missing":1,"complete":false}
  }
}
```

### 出力の例（最小の完結したJSONL）

次の例はまだ測定も割当も無い通常運用の選択範囲を出す場合で、空の結果も完了と欠測を混同しない。
実行を選んだ出力では、そのcondition・assignment・invocation・outcomeとEvidence・定義・値の行がmanifestとendの間に並ぶ。
`input_cut`の各storeの値はその窓で受理した位置、`ledger_version`は作ったビューの版である。
例を保存するときも各行末に改行を置く。

```jsonl
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000001","record_id":"manifest","kind":"manifest","required":true,"queue_id":"20000000-0000-4000-8000-000000000001","mode":"operational","data":{"selection":{"subjects":[],"window":null},"input_cut":{"event":0,"step":0,"node":0,"ledger_inputs":{"event":0,"step":0,"node":0},"ledger_version":1,"observed_at":"2026-11-17T00:00:00Z"},"created_at":"2026-11-17T00:00:00Z","mode_context":{"benchmark":null,"replay":null},"redaction_version":"1"}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000001","record_id":"end","kind":"end","required":true,"queue_id":"20000000-0000-4000-8000-000000000001","mode":"operational","data":{"record_count":1,"sha256":"affa97a7b3f0848048cbe1b29cc69b6bf2515b448b96234df9c4d2ad7f610bb2","complete":true}}
```

非空の例ではtimeoutした起動を落とさず、元の条件/割当/起動の本文は期限切れ、費用は未収集という違いを表す。
`common-contract-example`等は共通契約の実際の版の綴りを決める値ではなく例示用の参照である。
本文の種類は簡略化して同じEvidenceを参照するが、実際には記録ごとの由来と寿命を保つ。

```jsonl
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"manifest","kind":"manifest","data":{"selection":{"subjects":[{"type":"run","id":"r-demo"}],"window":null},"input_cut":{"event":12,"step":0,"node":0,"ledger_inputs":{"event":12,"step":0,"node":0},"ledger_version":1,"observed_at":"2026-12-18T00:00:00Z"},"created_at":"2026-12-18T00:00:00Z","mode_context":{"benchmark":null,"replay":null},"redaction_version":"1"}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"ev-record","kind":"evidence","data":{"subject":{"type":"run","id":"r-demo"},"category":"snapshot","source_refs":[{"type":"event","id":"12"}],"captured_at":"2026-11-17T00:01:00Z","content":{"ref":null,"digest":null,"bytes":null,"availability":"missing","missing_reason":"expired","expires_at":"2026-12-17T00:01:00Z"},"redaction":{"version":"1","applied":true,"reason":null}}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"condition","kind":"condition","data":{"experiment_ref":{"type":"experiment","id":"experiment-demo"},"experiment_version_ref":{"type":"experiment_version","id":"revision-demo"},"snapshot_ref":"ev-record","contract_ref":"common-contract-example"}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"assignment","kind":"assignment","data":{"assignment_ref":{"type":"assignment","id":"assignment-demo"},"condition_ref":"condition","variant_ref":{"type":"variant","id":"variant-demo"},"subject":{"type":"task","id":"task-demo"},"assignment_record_ref":"ev-record","origin_refs":[],"origin_refs_missing_reason":"not_applicable"}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"launch","kind":"invocation","data":{"invocation_ref":{"type":"invocation","id":"launch-demo"},"assignment_refs":["assignment"],"role":"worker","subject":{"type":"task","id":"task-demo"},"run":{"value":"r-demo","missing_reason":null},"attempt":{"value":1,"missing_reason":null},"launch_record_ref":"ev-record","input_refs":[],"output_refs":["ev-record"],"input_refs_missing_reason":"not_recorded"}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"outcome","kind":"outcome","data":{"invocation_ref":"launch","result":"timeout","reason":"provider_timeout","started_at":"2026-11-17T00:00:00Z","ended_at":"2026-11-17T00:01:00Z","evidence_refs":["ev-record"]}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"def-cost","kind":"measurement_definition","data":{"name":"invocation.cost","version":1,"unit":"USD","scope":"起動一回","method":"provider報告または固定価格snapshotによる換算。入力不足は欠測","inputs":["execution_tokens","provider_cost","price_snapshot"]}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"m-cost","kind":"measurement","data":{"definition_ref":"def-cost","subject_ref":{"type":"invocation","id":"launch-demo"},"window":{"start":"2026-11-17T00:00:00Z","end":"2026-11-17T00:01:00Z"},"input_cut":"manifest","value":null,"missing_reason":"not_recorded","estimate":{"kind":"unknown","basis_ref":null,"missing_reason":"not_recorded"},"source_refs":[{"type":"event","id":"12"}],"coverage":{"expected":1,"observed":0,"missing":1,"complete":false}}}
{"schema_version":"1.0","export_id":"10000000-0000-4000-8000-000000000002","required":true,"queue_id":"20000000-0000-4000-8000-000000000002","mode":"operational","record_id":"end","kind":"end","data":{"record_count":8,"sha256":"7ebb929c5b8a9ead16b70c91b7a27305005e7b6f7dc930c96a5841492861e7a8","complete":true}}
```

非空の出力では、例えば`condition -> assignment -> invocation -> outcome`を結び、成果物とreview判定のEvidenceを別々に参照する。
定義を先に出し、前掲の欠測Measurementや単価snapshot付きの換算Measurementを続ける。
個別testが取得できない場合も検証のoutcomeは残り、testのEvidenceは`unsupported`とする。
本文は別bundleとして明示して取得する場合だけ添え、通常のJSONLに本文を埋め込まない。

**根拠:** 生の本文を大量出力せず欠測と完了を機械で区別し、外部の結合と再分析を可能にするため。
**正本の移管先:** serializerの出力例とschemaのdoc comment。
実装時は非空・空・打切り・欠測・換算・版の拒否のfixtureで契約を検査し、この文書の例を削除して入口へ置き換える。

## 11. 保存・保持・容量・マスキングと認可の案

### 保存と既存の保持との接続

構造化した由来・利用可否の記録はEventStore、値は既存のLedgerStore/ReportStoreの派生、本文は有限保持の材料とする。
初期の本文は制御側が管理するqueue内の専用artifact領域に保存し、worktreeやagentが書けるrun dirを安全な置き場とみなさない。
外部object保存はadapterの後続拡張で、初期に必須にしない。
queue DBをPostgresにしても、本文がDBに入ったとは扱わない。
安全な受領を確定する前の参照はavailableにせず、書きかけやdigest不一致は欠測として扱う。

既存の`[retention]`の定義と既定値の入口は、`src/domain/retention.rs`の`RetentionConfig`と定義のそばのdoc comment。
既存の保持の値と対象は変更せず、run dirはその対象外のままにする。
Evidence用の有限保持を`[retention.evidence]`という追加表の案として分ける。
既存のstep・node資源・台帳・eventの保持も変更しない。
終了したrun dirの写し（goal 300）は別の保存元の計画で、このbaseの調査では完成したEvidence保管APIとは確認していない。
その写しの実装が取得元になる場合も、許可した内容だけを取り込み、同じ本文の二重保持を容量と期限の迂回にしない。
この契約はrun dir全体の削除/復元の規則も写しの形式も決め直さない。

### 既定値と上限の案

値は汎用の初期案で、このrepositoryの測定した容量でも運用設定でもない。
全項目を正の整数で受け、0/負値/overflow/無期限は設定の誤りにする。

| `[retention.evidence]`の項目 | 既定の案 | 根拠 |
| --- | --- | --- |
| `days` | 30日 | 直近の比較と調査のための有限の窓。長期の比較は構造化した値と許可済みの外部出力を使う |
| `artifact_bytes` | 16 MiB = 16,777,216 byte | 大きな出力一つで全体の枠を使い切らず、一般のテキストsnapshotを収容する |
| `subject_bytes` | 256 MiB = 268,435,456 byte | runまたはrunの無い計画の対象ごとに制限し、修正の多い対象による独占を防ぐ |
| `total_bytes` | 4 GiB = 4,294,967,296 byte / queue | 小規模運用でも保存量が無限に増えない初期の上限 |
| `export_temp_days` | 1日 | 中断したbundleの一時ファイルを後で回収できる有限の猶予 |

`artifact_bytes <= subject_bytes <= total_bytes`を検査する。
byteの量はマスキング後・圧縮前で数え、重複排除の有無で論理上限を広げない。
共有本文は参照する各対象の枠に数え、物理容量は別に観測する。
本文の期限は取り込み時刻から数え、再参照・再出力・archiveからの再取り込みで延長しない。
終了した対象の本文は期限または量の上限の早い方で回収し、量が超えたら期限の早い終了対象から回収する。
実行中の元資料は消さず、追加のEvidenceの受領を抑えて`size_limit`を記録する。
単体上限を超える本文は黙って切らず受領を断り、完全な本文が無いことを示す。
出力の一時bundleもtotalの枠へ数え、exportを反復して保持を延ばさない。
独立して人が持ち出した写しはDAGQから回収できないため、書き出し時に期限とマスク済みであることを付けるが遠隔削除は約束しない。

構造化した由来の最小記録は既存のeventの寿命に従い、本文の削除理由と元の期限を保持する。
受領の拒否や繰り返す失敗の記録自体も無限に増やさず、同じ対象/内容/理由の重複を排し、容量不足の細部を本文として監査へ逃がさない。
保存・取得・転送のbyte数を推論token/費用と別の測定定義で観測する。
保存単価を設定する新しい課金基盤は初期には作らず、不明な費用を0とせず、外部の単価による換算を推計として扱えるようにする。

**根拠:** survey §4・5の材料/cleanup、現行`RetentionConfig`、ADR-t5121-2の有限性と通常運用の保護。
**正本の移管先:** `RetentionConfig`に接続するEvidence設定の読取型・既定定数・容量検査と回収のdoc comment。
各項目を移した変更で、ここに具体値と設定keyを残さない。

### マスキングの対象と処理

| 対象の一覧 | 初期の扱いと理由 |
| --- | --- |
| API key・access/refresh token・password・Cookie・Authorization header・秘密鍵・署名URL | 値を収集しない。既存のenv注入と同じ資格情報名の判定に加え、本文内の既知の形も除去 |
| envと設定・CLI引数・URL query・接続文字列 | 許可した名前/種類だけ。secretの値と外部の宛先の資格部分を落とす。未知のenvを丸ごと保存しない |
| prompt・transcript・入力要求/計画・review/receipt・検証stdout/stderr | テキストのマスキングを通す。資格情報・個人情報・projectで指定した機密の形を除去。処理不能は`redacted`または`unsupported` |
| 人名・email・電話・住所・user名・host名・homeを含む絶対path・Git author | 分析に必要な型付きID/役割だけを残し、個人を識別する本文を置換。比較用のhostはqueue内の不透明な識別にする |
| diff・生成ファイル・snapshotの添付 | 許可したテキストだけを処理。secret file・credential store・binary・symlinkの外部参照・archiveの内部を既定で収集しない |
| hash/digestと設定版 | マスキング後の内容を識別する。秘密だけのhashは出さず、低エントロピー値を推測する辞書の手掛かりにしない |

collectorは許可した相対位置の通常ファイルだけを読み、path traversal・symlink・他対象への参照を拒む。
マスキング規則の版を受領時に固定し、元の原文を退避する迂回は持たない。
出力時には現在許可する規則で再検査し、旧規則の本文を再処理できなければ非開示にする。
再処理した本文は新しいdigestと規則版を持つ派生の安全な写しとして元への由来を残し、同じdigestを別内容へ使わない。
完全性を失ったsnapshotはReplay/Benchmarkの完全な入力の代わりにせず、必要な内容が保てるかを入力検査で判定する。
元の実行が読んだ秘密を収集しないので、実行全体の完全再現を保証しない。

**根拠:** survey §4のshapeは全ログの秘密除去を保証しないこと、[Security](../design/security.md)のデータと権限の境界、[監査](../design/audit.md)のsecretを残さない方針。
**正本の移管先:** Evidence受領型・マスキングpolicy/規則版の定義とexportの再検査のdoc comment。
これは通常のpromptの実行入力を変える設定ではなく、保存/提供の境界である。

### capabilityの名前とresourceの案

以下は追加の名前の案で、今のpolicyで許可されているとの説明ではない。
認可の実装時にauthorizationの正本と表へ同じ変更で移し、この表を値の無い入口へ置き換える。

| capabilityの案 | resourceと許可の案 | 根拠 |
| --- | --- | --- |
| `evidence.read` | user/inboxの認証した所有範囲内の対象・artifact。worker/planner/review-job等へ包括許可しない | 本文は通常の状態読取より機密の範囲が広い |
| `experiment.export` | user/inboxの所有範囲内の実験/対象/窓。本文を添付するなら`evidence.read`も必要 | 大量取得を単体読取から分け、統計だけでも対象の範囲を制約 |
| `evidence.manage` | user/inboxの所有範囲内の削除等の管理。保持を弱める変更は既存の人の直接の承認の規則に従う | agentが自己の保存/公開範囲を広げない |

制御側の受領と自動回収は既存の信頼する実行責務として行い、executor-telemetryのprincipalに本文取得・書き出し・管理を追加しない。
agentの申告した保存先やURLをserviceが代理取得しない。
対象内の項目の非開示以外、無権限の対象は件数も識別も出さず、出力の各本文の取得時にも再認可する。
人向けCLIとserviceのuse caseの双方が同じ認可を通り、bundleの直接pathで迂回できない置き場にする。

管理と書き出しの特権の許可/拒否は既存の監査policyへ接続し、対象参照・操作の結果・安全な件数とbyte数までに絞り、本文を監査へ写さない。
Evidence本文の期限切れで監査を消さず、auditの閲覧権限をEvidenceのcapabilityで代用しない。
SQLiteの同時確定、Postgresの段0の監査未対応、host同一UIDへの保証の限界は既存のままとし、新機能だけで強い監査があるとは表示しない。

**根拠:** [Authorization](../design/authorization.md)のcapability/resourceとdefault deny、監査の独立性、ADR-t5121-2。
**正本の移管先:** capability enum・use caseとresourceの対応・`StaticPolicy`のdoc comment。
本書は名前の案だけを持ち、既存の権限表や認証主体を変更しない。
