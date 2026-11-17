---
id: plan-agent-experiments-contract
type: plan
title: Agent比較実験の実装前の追加契約
status: active
created: 2026-11-17
related:
  - adr-t5120-1
  - adr-t5120-2
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
task 5121がEvidence・Measurement・書き出しの契約をこの文書へ追記するため、その欄と保持期間はここでは決めない。

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
