---
id: adr-0070
type: adr
title: open なtaskとgoalの完了見込み（forecast）をsimulationで出し、supervisorが決まったきっかけでsnapshotをeventに記録し、完了の時点で答え合わせをしてobserverが誤差を読む
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - observer
  - measurement
related:
  - adr-0038
  - adr-0047
  - adr-0049
  - adr-0051
  - adr-0079
  - design-supervisor-lifecycle-stats
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-kpi
---

# ADR-0070: open なtaskとgoalの完了見込み（forecast）をsimulationで出し、supervisorが決まったきっかけでsnapshotをeventに記録し、完了の時点で答え合わせをしてobserverが誤差を読む

## Context

[ADR-0051](0051-kpi-time-series-report-and-push.md)のKPIは終わったrunの流れを期間ごとに測るが、「今open なtaskやgoalがいつ終わりそうか」は出せない。人はgoalの進み具合を`graph`と`stats`の中央値から頭の中で見積もっていて、その見積もりが当たったかも残らない。見込みを出すだけでは、見積もり方が良いのかが分からないので、見込みを記録し、完了の時点で実績と比べる必要がある。

人の決定（2026-09-26、plannerとの対話。goal 40の下に置く）:

- snapshotの対象はすべてのopen なtaskとgoalにし、1回のsnapshotを1件のeventにまとめる。
- 見込みに流入（新しいtaskの登録、follow_upの率）を含めない。
- 記録は日次だけでは粗いので、計画の確定・変更の印・着地（見込みが一定以上動いたとき）・日次で取る。
- 人へのエスカレーションは無く、誤差はobserverが読む。

## Decision

**原則。** 見込みも答え合わせもLLMを使わず決まった規則で出す（ADR-0051の原則と同じ）。記録するのはその時点の見込みそのものだけで、答え合わせはeventから読むときに導く（[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定5）。数値の初めの値はこのADRに書くが、今の値と設定の名前は[stats](../design/supervisor-lifecycle/stats.md)の「完了見込み」が持ち、値の調整はこのADRを置き換えずに行ってよい（[ADR-t598-1](2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md)の決定2・3）。

1. **見込みはsrc/domainの純粋関数が、今のqueueと過去の所要時間の分布からsimulationで出す。**
   - **入力**: open なtask（`ready`と`in_progress`。`draft` / `submitted`はまだ確定した計画でないので含めず、`draft`のgoalに属する`ready`のtaskもclaimされないので含めない）、依存のグラフ（task依存と、[ADR-0038](0038-task-depends-on-a-goal-until-it-is-achieved.md)のgoal依存）、ADR-0049の決定4の効く優先度によるclaimの順、生きているsupervisorの`parallel`の合計、taskの種類（task 196の`kind`。nullは`unknown`）ごとの過去の着地したrunの`work`・`validate`・`wait_to_land`の分布（`stats`と同じ区間。`startup`は`work`の中に含まれるので足さない）、resumeと着地の延期の確率とその時間、人の答えの待ち（ADR-0051の`ask_wait`）、goalの最後のtaskの完了から`achieved`で閉じるまでの遅れの分布、今の時刻。
   - **計算**: 何度も（初めは1,000回）、空いたslotにclaimの順でtaskを入れ、`work` → `validate` → `wait_to_land`の時間を分布から引いて進め、resumeと延期は確率で段を足す。走っているrunは今の段と経過時間から、経過より長い標本だけを条件にして残りを引く（最初のcommitの前なら`startup`の経過も条件に使う）。人の答えを待つrunは[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)どおりslotを空け、残りの待ちを`ask_wait`から引く。1回ごとの完了時刻を並べ、taskごとのp50とp90を出す。goalの見込みは、そのgoalのopen なtaskがすべて完了した時刻に閉じるまでの遅れを足した時刻とし、goalに依存するtaskはその時刻まで待つ（ADR-0038）。
   - **流入は含めない**（人の決定）。新しいtaskの登録、follow_upのdraft、plan reviewの差し戻しは起きないものとして計算する。見込みは「今の計画がこのまま流れたら」の値になる。
   - **標本が少ない種類**は、その種類の標本が`min_samples`（ADR-0051の決定19）に満たなければ全体（`all`）の分布を使い、使ったことを出力に書く。
   - **同じ入力から同じ見込み**: 乱数の種を入力（今の時刻とqueueの最新のevent ID）から決め、同じ時点で何度計算しても同じ値にする。
   - 見積もり方には版（`method`）を付け、計算を変えるたびに上げる。答え合わせ（決定4）は版ごとに分けられる。

2. **`dagq forecast`は見込みを表示するだけで、記録しない。**
   読み取りのコマンド（read-onlyの接続）で、`--task` / `--goal`で絞れる。人が手で打った回数でsnapshotの時刻や数が偏らないように、記録は決定3のきっかけだけにする。observer・planner・plan reviewも読める（ADR-0047の決定4がobserverに許す読み取りのコマンドに加える）。

3. **snapshotはsupervisor（runtime）が決まったきっかけで記録し、LLMを使わない。**
   - **きっかけ**: (a) plan reviewのpass（そのproposalのgoalの基準の見込み）、(b) 変更の印（task 429 / ADR-0051の決定10〜12の記録する印、taskの優先度の変更、依存の変更。`parallel`は導く印でsupervisorは変化の時点を見られないので、supervisorの起動・引き継ぎの印で拾う）、(c) 着地のたび。ただし前のsnapshotからp50が一定以上動いたtaskかgoalがあるときだけ、(d) 日次（task 431の日次レポートと同じtimer）。
   - **(c)の閾値**: 前のsnapshotのそのtask / goalの残り時間（p50 − 前のsnapshotの時刻）の20%以上、かつ30分以上動いたとき（初めの値）。小さな揺れで記録が着地の数だけ増えないため。
   - 同じsupervisorの周回の中で重なったきっかけは1件のsnapshotにまとめ、きっかけをすべて載せる。queueで書くsupervisorは1つ（日次レポートと同じ）で、run slotを使わない。
   - **1件のevent**（例: `forecast_recorded`。名前は実装taskが決める）に、すべてのopen なtaskとgoalのp50 / p90と、前提（`parallel`、種類と段ごとの分布の標本数、全体の分布に代えた種類、試行の回数、`method`、分布を読んだ範囲のcursor）ときっかけを持たせる。KPIの記帳のeventとして扱い、observerを起こさない（ADR-0051の決定24）。

4. **答え合わせはtaskの完了とgoalのcloseの時点で、その対象のsnapshotすべてに実績を当てる計算を、`stats` / `kpi`を読むときにeventから導く。**
   - **標本**はsnapshotと対象の組。taskは`completed`になった時刻、goalは`achieved`で閉じた時刻を実績にする。`canceled`のtaskと`abandoned`で閉じたgoalは実績が無いので数えず、件数だけ出す。
   - **指標**: p50の誤差（実績 − p50の秒と、見込みの残り時間に対する比）の中央値と偏りの向き（遅れ側・早い側の割合）、p90の的中率（実績がp90以前だった割合）、見込みの残り時間の帯ごとの誤差、taskの種類ごとの誤差、`method`ごとの誤差。
   - **計画の変更と見積もり方法の誤差を分ける**: snapshotから完了までの間の変更の印（決定3の(b)と、読むときに導く`parallel`などの導く印）の数を標本ごとに並べ、印が0の標本だけの誤差も出す。印の無い標本の誤差を見積もり方法の誤差、印のある標本との差を計画の変更によるずれとして読む。
   - KPIの期間には完了の時刻で入れ、ADR-0051の決定1の表の形（入力、分子と分母、期間への入れ方、良い向き）でKPIとして足す（ADR-0051の決定1が規則を変えずに足すことを許している範囲）。良い向きは、p50の誤差の秒の絶対値は小さい、p50の誤差の比の中央値とp90の的中率は目標の範囲（比は0の近く、的中率は90%の近く。`candidates`と同じ形）にする。

5. **誤差を読むのはobserverで、偏りが続けばfindingにし、人には上げない。**
   - 「偏りが続く」は、答え合わせのKPIのADR-0051の決定18の目標割れ（`breach`）にする。別の判定の経路は作らない。目標はADR-0051の決定17どおりruntimeに埋め込まず、実装が着地した後にplannerが`dagq.toml`の`[kpi.targets]`に書く（初めの目標の案: p50の誤差の比の中央値が±25%以内、p90の的中率が75%以上）。目標割れの始まりと解消はADR-0051の決定18のeventとして記録され、pushも同じ規則で動く。observerは数字を作らず、この判定を引く。
   - observerは答え合わせのKPIの目標割れを、種類`kpi`ではなく種類`forecast`のfinding（ADR-0047の決定18の種類に足す）にし、`subject`を指標と層（例: `p50_bias/kind=runtime`）にする。計算の改善が要ればproposalを求める印を付け、ADR-0047の決定19の経路でruntimeが立てるplannerが計算の改善をproposalにする。改善のproposalの上限と優先度はADR-0051の決定25・26のまま。`blocked`のaskにはしない。
   - 日次レポート（task 431、ADR-0051の決定20）に見込みの誤差の欄を足す（直近の期間の指標と偏りの判定）。pushの中身（決定23）は変えない。

## Alternatives

- **過去のrunの中央値を足し合わせる決定的な計算にする。** 簡単だが、並列度と依存による待ち、resumeの裾が表せず、p90が出ない。
- **見込みに流入を含める。** 実際の完了には近づくが、流入の率そのものの予測が誤差に混ざり、見積もり方法の良し悪しを読めなくなる（人の決定で含めない）。
- **plan reviewの重さの予測（ADR-0079）を分布の選び方に使う。** 順位は予測できるが値は2〜3倍に偏るので、まずは種類ごとの分布で答え合わせの基準を作る。使うときは`method`を上げ、答え合わせで比べる。
- **`dagq forecast`を打つたびに記録する。** 手で打った回数と時刻で標本が偏るので、記録はきっかけの決まったsupervisorだけにする。
- **snapshotを日次だけにする。** 計画の確定や変更の直後の見込みが残らず、変更によるずれを分けられない（人の決定）。
- **着地のたびに必ず記録する。** 見込みが動かない着地でもtask全部の行が残り、eventが着地の数だけ太る。

## Consequences

- goalとtaskの完了の見込みが、今の計画がこのまま流れた場合の値としていつでも読め、見積もり方の当たり外れが種類・残り時間・`method`ごとにKPIとして残る。
- 1件のsnapshotはopen なtaskとgoalの数だけ行を持つので、eventが大きくなる。(c)の閾値で数を抑える。
- 見込みには流入が入らないので、follow_upの多いgoalでは実績が見込みより遅れる向きに偏る。印の無い標本の誤差と並べて読む。
- 計算を変えると`method`が上がり、前の版の答え合わせは前の版の標本だけで読む。

実装は未着手。見込みの計算と`dagq forecast`、snapshotの記録、答え合わせのKPI、observerの判定と日次レポートの欄は、goal 40の後続taskが持つ。
