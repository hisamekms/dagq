---
id: adr-t609-1
type: adr
title: 生きているrunのalertで復旧jobが失敗したら、recover by handのattentionではなく、そのalertのaskを開く（ADR-0047決定40をamends）
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
amends:
  - adr-0047 decision 40
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - operations
related:
  - adr-0047
  - adr-t598-1
  - design-supervisor-lifecycle-background-recovery-job
  - design-supervisor-lifecycle-triage
---

# ADR-t609-1: 生きているrunのalertで復旧jobが失敗したら、recover by handのattentionではなく、そのalertのaskを開く（ADR-0047決定40をamends）

## Context

[ADR-0047](0047-irregularities-in-three-layers-recovery-job-ask-reasons-and-goal-review.md)の決定39・40は、復旧jobを生きているsessionのalertにも広げ、task 441はそのうち3つ（backgroundの処理が長く動き続ける、`/exit`で終わらない、既知でないダイアログで止まる）で生きているsessionの復旧jobを実装した（receiptの無いidleは別の検知のaskのまま）。決定40は、jobがescalateを返したときはそのalertのaskを開き、jobそのものが失敗したとき（起動できない、非0で終わる、時間切れ、verdictが形に合わない）は、終わったrunのtriageの失敗と同じく、対象を動かさずにinbox宛ての`recover by hand`のattentionにすると決めていた。

task 441はこの決定どおりに着地した。その最初の実装は、jobの失敗でもそのalertのaskを開いており、人はinbox経由でその形を承認していた（ask 109）が、補足がworkerに届く前にresumeが終わり、ADRどおりのattentionの形で着地した。

2026-09-26に人は、生きているrunではaskの形にすると決めた（inbox経由の依頼、task 608のplanner_questionのask 117の答え）。理由は次のとおり。

- 生きているrunには、attentionに対する人の判断を適用する経路が無い。人はworkspaceの画面を読み、手でキーや`/exit`を送らなければならない。
- askなら、人はalertの今の選択肢（`/exit`で終わらないなら終わらせるか待つか、など）で答えるだけで済み、supervisorがその答えを今のaskと同じ経路で適用して処理を続けられる。

## Decision

1. **生きているrunのalertで復旧jobが失敗したら、そのalertのaskを開く。**
   - 対象は、生きているsessionで復旧jobを起動するalert（今はbackgroundの処理が長く動き続ける、`/exit`で終わらない、既知でないダイアログで止まるの3つ）だけ。
   - 失敗は、jobが起動できない、非0で終わる、時間切れになる、verdictが無いか形に合わない場合。
   - 開くaskは、jobがescalateを返したときにそのalertで開くのと同じask（同じkindと選択肢）で、人が要る理由は復旧の失敗、questionには復旧jobが失敗したこととその理由を書く。答えの適用、そのaskを待つ間に同じalertのjobを立てないこと、sessionが動いたときの閉じ方は、そのalertの今のaskに従う。
   - `recover by hand`のattentionは、生きているrunのalertのjobの失敗では作らない。
2. **終わったrunの復旧job（triage）の失敗は変えない。** 失敗・中断・resumeの使い切りで終わったrunのjobの失敗は、今までどおり対象を動かさずにattention（`triage by hand`、`recover by hand`と読む）にする。終わったrunには`ready`やcancelなどを打つ経路が既にあり、runの記録から人が判断できるからである。

ADR-0047の決定40のうち、jobの失敗をattentionにする部分を、生きているrunのalertについてだけこれで改める。決定40のその他（verdictの形、許された操作と許されない操作、escalateのaskのkindと選択肢、自信が無いときの扱い、適用した操作の記録）は変えない。人が要る理由の分類（決定41）も変えず、開くaskは復旧の失敗に分類する。

## Alternatives

- **attentionのままにする（ADR-0047のまま）**: 人がworkspaceを見て手でキーや`/exit`を送る必要が残り、答えをsupervisorに渡す経路が無い。人はask 109で既にaskの形を承認していた。
- **jobの失敗のための新しいaskのkindを作る**: 答えの適用・待ちの間の抑止・閉じ方をalertごとに二重に持つことになる。人が選ぶことはescalateのときと同じなので、そのalertの今のaskを使う。
- **終わったrunのtriageの失敗もaskにする**: 終わったrunには人が打つコマンドの経路があり、今回の問題（生きているsessionに判断を適用する経路が無い）に当たらない。変える理由が無いので広げない。

## Consequences

- 生きているrunでjobが失敗しても、人はinboxでaskに答えるだけで済み、手でキーや`/exit`を送る場面が減る。
- 人の答えを待つrunとしてslotの扱い（[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)）もそのalertのaskに従う。
- 実装はtask 562が行う。記録するeventやaskの欄、status・attentionの判定の変更、testは[生きているsessionの復旧job](../design/supervisor-lifecycle/background-recovery-job.md)と、そこから辿るdesignの文書が持つ。
