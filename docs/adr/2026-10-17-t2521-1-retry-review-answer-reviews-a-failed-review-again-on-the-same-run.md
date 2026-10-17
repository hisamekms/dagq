---
id: adr-t2521-1
type: adr
title: reviewが失敗してverdictの無いrunのapprove_landingのaskにretry_reviewを足し、答えるとruntimeがsessionを再開せず同じrunとreceiptでreviewを起動し直す
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
owners:
  - hisamekms
tags:
  - runtime
  - review
  - ask
related:
  - adr-t1207-1
  - adr-t1984-1
  - adr-0054
  - adr-0068
  - adr-t1847-1
  - design-supervisor-lifecycle-review
---

# ADR-t2521-1: 失敗したreviewのapprove_landingに、同じrunとreceiptでreviewをやり直すretry_reviewを足す

## Context

reviewが失敗してverdictが無いと、自動の1回のやり直し（[ADR-t1984-1](2026-10-07-t1984-1-retry-a-review-whose-job-exited-non-zero-once.md)）の後に`approve_landing`のaskが開く（[ADR-t1207-1](2026-09-30-t1207-1-codex-run-review.md)決定3）。
askの選択肢は`land`・`send_back`・`cancel`だけで、providerのログイン切れや利用上限のように、変更に問題が無くreviewだけをやり直したいときも、人は「変更せずreceiptを出し直す」の`send_back`にするしかなかった。
`send_back`はworkerのsessionをresumeしてslotを1本ずつ使うので、失敗が重なるとresumeがslotを取り合い、答えから着地まで数時間かかった。

## Decision

1. **失敗したreviewのaskだけが`retry_review`を持つ。** reviewが失敗してverdictが無いときに開く`approve_landing`のaskは、選択肢に`retry_review`を足し、questionにその意味（同じrunとreceiptでreviewをやり直す。変更もsessionの再開もしない）を書く。
   verdictの`concern`・`revise`の上限・agentsのrouteから開くaskの選択肢と推奨・確信度（[ADR-0040](0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)、[ADR-t451-1](2026-10-02-t451-1-ai-decides-recommendable-asks-and-escalates-only-the-undecidable.md)決定3）は変えない。
2. **受け付けの判断は1つで、askの選択肢で決める。** askが失敗したreviewから開いたかは、そのaskが`retry_review`を選択肢に持つかで決める。
   askの記録だけで決まり、runのeventを読み直さず、後から別のreviewが走っても答えたaskの意味が変わらないためである。
   この判断を、回答の記録（runtimeが配るか）・表示（答えを適用中か、inboxが読むか）・適用の3か所が同じ1つの判断として使う。
   verdictのaskへの`retry_review`はruntimeが配らず、適用せず、inboxが読む答えとして残る。
3. **sessionを再開せず、同じ入力で起動し直す。** 適用はaskを閉じ、そのaskの`retry_review`を適用したことを1回だけ記録し、runを`awaiting_integration`のままreviewの待ちに置く。
   その後のpassが、空きslotがあるときにleaseを取り、validationを通ったrunと同じ入力（今のreceiptとlanding branch）で次のattemptのreviewを起動する。
   workerのsessionは再開せず、receiptの出し直しも求めない。
   slotが無ければ次のpassを待ち、記録が1回なので答え1つでreviewは1回だけ起動する。
   起動の後は他のreviewと同じで、providerの控え（[ADR-t1847-1](2026-10-08-t1847-1-review-waits-while-the-provider-that-runs-its-subagents-is-held.md)）なら待ち、動かせるproviderが無ければ失敗と新しい`approve_landing`のaskになり、自動の1回のやり直しもそのまま働く。
4. **reviewの理由の集計に入れない。** 失敗したreviewのaskの答えは理由の集計（[ADR-t947-1](2026-09-28-t947-1-review-verdicts-carry-reason-codes.md)）に記録しないので、`retry_review`も記録しない。

## 変えない決定との関係

- ADR-t1207-1決定3（ADR-t1984-1が改めた形）の「一般の失敗は1回の自動のやり直しの後に手動review（`approve_landing`）に渡す」はそのままで、この決定はそのaskの答えに選択肢を1つ足すだけである。
- [ADR-0054](0054-run-lease-ownership-parallel-supervisors-and-recover.md)決定6の「reviewが失敗してaskを開いたらleaseを返し、`awaiting_integration`で答えを待つ」はそのままで、答えの適用の後にleaseを取り直すのは、resumeやtriageがleaseを取り直すのと同じ次の担い手の扱いである。
- [ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)決定4の「閉じていない`approve_landing`のaskがあるrunはresumeの後にreviewをやり直さない」は、askを閉じてからreviewを起動し、resumeもしないので当たらない。

## Alternatives

- runの最後のreviewのevent（`review_failed`か）で決める: askを開いた後に別のreviewが走ったときに、古いaskの答えの扱いが読み方で変わる。選択肢はaskを開いたときに決まるので選択肢で決めた。
- 答えをworkerのresumeに渡し、sessionがreceiptを出し直してからreviewする（今の`send_back`）: slotをresumeに使い、変更の無いturnが走る。これを避けるのがこの決定の目的である。
- 全ての`approve_landing`のaskに`retry_review`を出す: verdictのあるreviewのやり直しは結果を変えないことが多く、人のverdictへの判断を飛ばす経路になる。要るとわかったら別に決める。

## Consequences

- providerの一時的な失敗によるreviewの失敗は、人が`retry_review`と答えるだけで、resumeとslotの取り合い無しに着地まで進む。
- この決定の前に開いた失敗したreviewのaskは選択肢に`retry_review`を持たないので、その答えはinboxが読む。
- 既存の番号付きの決定を変えないので、amendsを持たない独立した決定にした。
