---
id: adr-t947-1
type: adr
title: reviewとplan reviewのjobがverdictのreasonsごとに分類コードを付けてruntimeが記録し、jobが判定できない分類は人のapprove_landing / approve_planの答えからruntimeが補う
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
owners:
  - hisamekms
tags:
  - runtime
  - review
  - planning
  - measurement
related:
  - adr-0027
  - adr-0049
  - adr-0047
  - adr-t728-1
  - adr-t876-1
  - adr-t947-2
  - adr-t947-3
  - adr-t947-4
  - design-supervisor-lifecycle-review
  - design-supervisor-lifecycle-plan-review
  - design-supervisor-lifecycle-stats
  - plan-review-sendback-reasons
---

# ADR-t947-1: reviewとplan reviewのjobがverdictのreasonsごとに分類コードを付けてruntimeが記録し、jobが判定できない分類は人のapprove_landing / approve_planの答えからruntimeが補う

## Context

headlessのreview（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)がADR-0040から引き継ぐ）とplan review（ADR-0044から[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)が引き継ぐ）のverdictは、`verdict`・自由文の`reasons`・`summary`だけを持つ。差し戻しの理由は、後から文を1件ずつ読んで分類するしかない。

task 945の分析（[review-sendback-reasons](../plans/review-sendback-reasons.md)）では、reviewにかかったrunの11.0%がreviseかconcernを受け、concernの人の答え待ちは計2,963分、send_backの後のresumeは1,009分だった。主な理由は受け入れ条件とADR・事実の食い違い（concern）と、局所の誤り・文書の取り残し・testの不足（revise）で、減らす手は理由ごとに違う。concernの68%は人が`land`と答えたが、それが「reviewの誤り」か「逸脱の追認」かはverdictの時点では判定できない。plan reviewのreviseの理由は、依存の欠け・taskの重複・古いADRの参照など、runのreviewと重なる所が小さかった。

goal 64は、差し戻しが起きたときに分類コードを記録し、stats と kpiで分類ごとの件数・延びた時間・率を読めるようにすると決めた（2026-09-28、人とplanner）。

## Decision

1. **jobがreasonsの項目ごとに分類コードを付ける。** reviewとplan reviewのverdictの`reasons`の各項目は、自由文に加えて1つ以上の分類コードを持つ。先頭が主のコードで、2つ以上に当たるときは直すのに要る判断の重い方を主にする。どれにも当たらなければ`other`にし、自由文で説明する。verdictの主の理由は、verdictを決めた最初の項目の主のコードとする（止めない注意の項目は主にしない）。
2. **reviewとplan reviewは別の集合を持ち、同じ種類の問題には同じ名前を使う。** runのreviewは実装とreceiptの問題、plan reviewはtaskとtaskどうしの関係の問題を分類するので集合を分ける。受け入れ条件どうし・条件とADR・条件と事実の食い違いのように両方が見る問題は、両方の集合と[ADR-t947-2](2026-09-28-t947-2-worker-questions-carry-topic-codes.md)のworkerの問いで同じ名前と定義を使い、plan reviewで止められなかったものが後でreviewやworkerの問いになった割合を1つの集計で追えるようにする。
3. **verdictはデータのままで、記録するのはruntime。** jobは今までどおり状態を変えるコマンドを打たず（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)）、コードはverdictのデータの一部としてruntimeが`review_finished` / `plan_review_finished`に記録する。コードが欠けた・集合に無い項目があっても、verdictは捨てずに適用する（欠けは「未分類」、知らない値はその値のまま記録する）。コードの有無はpass・revise・concernの判定と適用に影響しない。
4. **jobが判定できない分類は、人の答えからruntimeが補う。** reviewの指摘が誤りだったか、人が逸脱を受け入れたかは、concernの`approve_landing`と`approve_plan`の答えで分かる。runtimeは答えを適用するときに、答えの結果をそのaskと差し戻しに結び付けて記録する: `land`・`ready`は「逸脱を受け入れた（reviewの誤りの候補を含む）」、`send_back`は「指摘が直す価値のあるものだった」でverdictのコードを引き継ぐ、`cancel`は取りやめ。人にコードを選ばせることはしない。reviseには人の答えが無いので補わない。
5. **stats と kpiが分類ごとに読めるようにする。** `stats`は主のコードとコードの集合ごとに、差し戻しの件数、差し戻しを受けたrun / proposalの率、reviseの手直し・concernの答え待ち・send_backの後のresumeの時間、答えの結果の内訳を出す。`kpi`は差し戻しの率を分類ごとの系列として出す。時間は主のコードにだけ付け、件数は集合で数える。コードの無い過去の記録は書き換えず「未分類」として数える。
6. **コードの一覧と定義はdesignが持ち、ADRなしに足し引きできる。** 一覧は分析と運用の結果で変わる見込みなので、ADR-0047決定41の`reason_category`（値を足すにはADRが要る）とは違い、designと実装の変更で足し引きする。コードはlabelとして記録し、知らない値も読める（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)決定3のlabelの列）。一覧はdagqが定義するが、コードは集計のためだけにあり状態遷移にも適用にも使わないので、同決定がfail closedにする状態や人の判断の分類には当たらない（読み違えても集計の行が1つ増えるだけで、動作は誤らない）。一覧・定義・主の選び方の順・欄名は[Review](../design/supervisor-lifecycle/review.md#差し戻しの分類コード未実装)と[Plan review](../design/supervisor-lifecycle/plan-review.md#差し戻しの分類コード未実装)、集計の出力は[Stats](../design/supervisor-lifecycle/stats.md#分類コードごとの集計未実装)が持つ。

## Alternatives

- **今のまま自由文だけにする**: 分類のたびに人かagentが全文を読み直す。task 945の分類は1人が数時間かけて付けたもので、続けて測れない。
- **記録は自由文のまま、後から分析だけで分類する**: 読み手ごとに分類が揺れ、期間をまたいだ比較ができない。reviewのjobは指摘を書く時点で文脈（receipt・diff・ADR）を持っており、そこで付けるのが最も安い。
- **人が付ける**: concernの答えのたびに人がコードを選ぶと、答えの手間が増えて夜の答え待ちが延びる。reviseは人を通らないので付けられない。人の答えから分かることは、答えそのもの（land / send_back / cancel）から補えば足りる。
- **reviewとplan reviewで1つの集合にする**: plan reviewの主な理由（依存の欠け・重複・登録の形式）はrunに無く、runの主な理由（局所の誤り・testの不足）はtaskに無い。1つにすると、どちらの集合にも使わないコードが並び、jobの判定が揺れる。

## Consequences

- reviewとplan reviewのpromptとverdictのschemaにコードの欄が加わり、jobはコードの定義を読む。定義が曖昧だと付け方が揺れるので、designの定義に判定の例を置く。
- `approve_landing`の`land`は「reviewの誤り」と「逸脱の追認」を区別しない。区別が要ると分かれば、答えの理由のコードを人に選ばせるかを別のADRで決める。
- 実装（verdictの欄、記録、答えからの補い、stats と kpiの出力）はgoal 64の別のtaskで行う。着地するまで、verdictは今の形のままである。
