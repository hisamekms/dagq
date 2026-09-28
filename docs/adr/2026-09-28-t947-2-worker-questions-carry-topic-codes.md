---
id: adr-t947-2
type: adr
title: workerがworker_questionのaskを打つときに問いの中身の分類コードを付け、ADR-0047決定41のreason_categoryと併せ持つ
status: accepted
created: 2026-09-28
updated: 2026-09-28
accepted_on: 2026-09-28
owners:
  - hisamekms
tags:
  - runtime
  - ask
  - worker
  - measurement
related:
  - adr-0022
  - adr-0047
  - adr-t728-1
  - adr-t876-1
  - adr-t947-1
  - design-supervisor-lifecycle-ask
  - design-supervisor-lifecycle-stats
  - plan-worker-question-topics
---

# ADR-t947-2: workerがworker_questionのaskを打つときに問いの中身の分類コードを付け、ADR-0047決定41のreason_categoryと併せ持つ

## Context

workerは判断が要るとき`dagq ask --kind worker_question --because <scope|discard>`を打つ（ADR-0022決定2、[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)決定41）。`reason_category`は「なぜ人が要るか」を表し、askにしてよいかの関門とinboxの見せ方に使うが、worker_questionはほぼ全てが`scope`になり、「何が決まらなかったか」は問いの文を読まないと分からない。問いの文は`events`のpayloadに無く、`dagq asks --all`でしか読めない。

task 950の分析（[worker-question-topics](../plans/worker-question-topics.md)）では、worker_questionは13件（runの3.0%）、答えまで計896分で、夜の5件が535分を占めた。分類はADRとの食い違い・条件が事実のため満たせない・範囲の外の変更・他のtaskとの重なり・前提の未着などに分かれ、どれをworkerが自分で決めてよいことにするかで減らせる時間が違う（候補を当てはめると8件・約583分）。件数は少ないが、1件あたりの人の時間が長く、分類がないと規則を変えた効果を測れない。

goal 64は、worker_questionも起きたときに分類コードを記録すると決めた（2026-09-28、人とplanner）。

## Decision

1. **workerが問いの中身の分類コードを付ける。** `worker_question`のaskは、主の分類コードを1つ必ず持ち、副のコードを0個以上持てる。主はworkerが止まったきっかけ（最初に満たせなくなったもの）、副はそれを解くのに一緒に決める必要があるもの。どれにも当たらなければ`other`にし、問いの文で説明する。付けるのは問いを書く時点のworker自身で、問いの文脈を最もよく知る者である。
2. **`reason_category`は置き換えず、併せ持つ。** `reason_category`（ADR-0047決定41。値を足すにはADRが要る）はaskの関門と見せ方のまま残し、分類コードは減らす手を選ぶための集計に使う別の軸にする。runtimeは一方から他方を推さず、両方をそのまま記録する。成果を捨てるかの問いと`discard`のように対応の目安はdesignに置くが、食い違っても拒否しない。
3. **「workerが決めてよいこと」を示すコードも受け付ける。** 条件・ADR・範囲に触れない実装の選び方は、AGENTS.mdではaskにせずworkerが決めるものだが、そう分類した問いもaskとして受け付けて記録する。拒むとworkerが別のコードで偽って打つので、数えて、workerへの指示とpromptの直しどころにする。
4. **記録はruntimeが行い、stats と kpiが分類ごとに読めるようにする。** runtimeはコードを`ask_opened`のpayloadと`asks`の出力に載せる。`stats`はコードごとに件数・runに対する率・答えまでの時間・答えの後の経過（着地・failed・reviewのconcern）を出し、`kpi`は答え待ちの時間を分類ごとの系列として出す。コードの無い過去のaskは書き換えず「未分類」として数える。
5. **一覧と定義はdesignが持ち、ADRなしに足し引きできる。** runのreviewとplan reviewの集合と同じ種類の問題には同じ名前を使う（[ADR-t947-1](2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)決定2）。コードはlabelとして記録し、知らない値も読める（[ADR-t876-1](2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)決定3）。一覧・定義・主の選び方・flagの綴りは[`ask` / `answer` / `asks`](../design/supervisor-lifecycle/ask.md#worker_questionの分類コード未実装)が持つ。
6. **対象は`worker_question`だけにする。** runtimeとjobが作るask（`approve_landing`・`decide`など）は、kindと出どころのverdictのコード（ADR-t947-1）ですでに分類される。`planner_question`に広げるかは、件数と中身を見て別に決める。

## Alternatives

- **今のまま自由文だけにする**: 問いの文は`asks --all`でしか読めず、規則を変えた前後の比較のたびに全文を読み直す。
- **後から分析だけで分類する**: task 950の分類は1人が付けたもので、境（ADRとの食い違いか条件の不能か）に判断が入る。問いを書いたworkerが付けるほうが揺れが小さい。
- **人が付ける（答えるときにinboxか人が選ぶ）**: 答えの手間が増え、夜の答え待ちを延ばす。問いの中身は問う側が一番よく知っている。
- **`reason_category`の値を増やして中身も表す**: `reason_category`はaskにしてよいかの関門で、値を足すにはADRが要る。関門と集計の軸を1つの欄に混ぜると、分類を細かくするたびに関門の規則が変わる。

## Consequences

- workerのpromptとCLIの`ask`に分類の欄が加わる。worker_question以外のkindに付けたときの扱い（拒むか無視するか）はdesignで決める。
- workerの推奨の答え（task 950では推奨を書いた10件が全て推奨どおりに答えられた）を別の欄に持たせる案は、この決定に含めない。
- 実装（CLIの欄、記録、stats と kpiの出力、promptの定義）はgoal 64の別のtaskで行う。着地するまで、worker_questionは今の形のままである。
