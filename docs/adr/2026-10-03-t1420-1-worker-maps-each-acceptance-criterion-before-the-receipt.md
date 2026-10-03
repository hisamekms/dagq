---
id: adr-t1420-1
type: adr
title: workerはreceiptの前に受け入れ条件の各項目を満たすものへ対応づけ、何も指せない項目はその場で直し、満たせない項目はfollow_upに回さずworker_questionかfailedのreceiptにする
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
owners:
  - hisamekms
tags:
  - runtime
  - worker
  - review
related:
  - adr-0047
  - adr-0029
  - adr-t947-3
  - design-supervisor-lifecycle-prompt
  - design-supervisor-lifecycle-review
  - plan-review-sendback-reasons
---

# ADR-t1420-1: workerはreceiptの前に受け入れ条件の各項目を満たすものへ対応づけ、何も指せない項目はその場で直し、満たせない項目はfollow_upに回さずworker_questionかfailedのreceiptにする

## Context

2026-10-02の人のreviewの監査（goal 90）: 03:07Z（印60152、runのreviewをCodexに切り替えた）以降にreviewにかかった35 runのうち26 runがreviseかconcernで差し戻され、理由が`acceptance_unmet`のものは16 run・27 verdictだった。16 runの理由をtaskのacceptanceと照合すると、明確な根拠の無い指摘は0件で、実装・集計の漏れ（task 1161・1368）、証拠・出力の漏れ（1274・1318）、測定の環境や広すぎる条件との食い違い（961・1075）だった。workerがreviewに渡す前に気づけたはずの漏れが、reviewの往復（再review・手直し・着地の遅れ）になっている。

[review-sendback-reasons](../plans/review-sendback-reasons.md)の「減らせる手の候補」は、1でworkerがreceiptの前に条件ごとの対応を自分で確かめること、2でworkerが逸脱を決めた時点で`worker_question`（`--because scope`）を出すこと（AGENTS.mdの今の規則の徹底）を挙げ、まだ実装していなかった。concernの22回のうち21回は逸脱をreceiptに開示しており、作業中に問いを出したのは1件だけだった。

## Decision

1. **workerはreceiptを書く前に、受け入れ条件を項目ごとに、それを満たすもの（変えたファイル、testの名前、receiptのevidence、文書の節や測るコマンド）へ対応づけ、何も指せない項目はその場で直す。** resumeとreviseの後に書き直すreceiptも同じで、直した項目の対応を書き直す。
2. **taskの中で満たせない項目をfollow_upに回してsucceededのreceiptを書かない。** 人の判断が要るもの（受け入れ条件や範囲が変わる）は`worker_question`（`--because scope`）、範囲の外の作業が要るものはfailedのreceiptに理由を書く。これはAGENTS.mdとpromptの今のaskの規則の徹底で、新しい規則ではない。
3. **対応はreceiptの`summary`に項目ごとの短い句で書く。** receiptのschemaは変えない。
4. **手順はruntimeのworkerのprompt・resumeの依頼・reviseの依頼に短い文で入れ、新しいtestの実行や検査のコマンドをworkerに求めない。** runのreviewのpromptと判定の基準は変えない（goal 90の測定でworker側の変化だけを見るため）。

文面・定数の名前・文字数は[Prompt](../design/supervisor-lifecycle/prompt.md#受け入れ条件の対応づけ)が持つ。

## Alternatives

- **長いチェックリストをpromptに足す**: 読む量が増え、守られにくい。項目の対応づけという1つの手順に絞る。
- **receiptに新しい欄（条件ごとの対応）を足す**: schema・validation・reviewの変更が要る。この手順の効果をgoal 90の測定で見てから決める。
- **workerに全体のtestを流させる**: AGENTS.mdのworkerのtestの規則（関係するmoduleだけ、全体は`integrate`の関門が1回）に反し、hostのloadを上げる。漏れの多くはtestの失敗でなく、条件の項目の取りこぼしである。

## Consequences

- 初回のreviewの差し戻し（特に`acceptance_unmet`）と着地までの時間が減ることを期待する。効果はgoal 90の前後比較で見て、本数が少ないので率の差を因果と言い切らない。
- workerのsummaryが条件の項目の数だけ長くなる。
- 範囲の外や人の判断が要る項目は、reviewの差し戻しより前にaskかfailedのreceiptとして表に出る。
