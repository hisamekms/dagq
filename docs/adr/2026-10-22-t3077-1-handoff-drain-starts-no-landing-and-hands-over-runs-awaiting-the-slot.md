---
id: adr-t3077-1
type: adr
title: 自動更新のhandoffのdrainは着地を始めず、着地slotを待つrunを保留の有無に依らずleaseを持ったまま引き継ぎ、既に始まっていた着地を待つあいだもworkerのreceiptを観測する（ADR-t3048-1決定3をamends）
status: accepted
created: 2026-10-22
updated: 2026-10-22
accepted_on: 2026-10-22
amends:
  - adr-t3048-1 decision 3
amended_by:
  - adr-t3344-1
owners:
  - hisamekms
tags:
  - runtime
related:
  - adr-t3048-1
  - adr-0045
  - adr-0039
  - adr-t1091-1
  - design-supervisor-lifecycle-handoff
---

# ADR-t3077-1: 自動更新のhandoffのdrainは着地を始めず、着地slotを待つrunを保留の有無に依らず引き継ぐ（ADR-t3048-1決定3をamends）

## Context

handoffのdrainは、DBとrunのファイルから組み立て直せないslot（検証・e2e・着地が進行中のもの）だけを進め、全slotが組み立て直せるようになったらexecする。
ADR-t3048-1決定3は、着地slotを待つrunのうち着地が保留のものだけを引き継ぎに数え、保留の無いものは「すぐ着地を始めるので、待っても短い」（同Alternatives 3）として、drainがその着地を始めて終わりまで待つとした。

本番の記録はこの見積もりに合わない。
着地の検証は15〜20分かかり、handoffの上限1800秒に迫る。
2026-10-18 01:53:25Zに頼まれたhandoffは、頼まれた後の02:00頃に着地を始めて02:18:59まで待ち、その間に終わったworkerのreceiptは約10分観測されなかった。
同日10:04:27Zのhandoffは上限で失敗し（`update_failed`）、receiptの観測は約22分遅れた。
10-19 00:04:35.996Zの要求では、その0.3秒後に着地を始めた。
drainは組み立て直せるslotを進めないので、走るworkerのslotのreceiptは、drainが待つあいだ観測もvalidationもされない。

## Decision

1. **handoffのdrainは着地を始めず、着地slotを待つrunを保留の有無に依らず引き継ぐ。**
   handoffのdrainに入ったsupervisorは、着地slotを待つrunの着地（とその前のe2e）を始めず、そのrunをleaseを持ったまま引き継ぐslotに数える。
   次のprocessは、ADR-t3048-1決定3の保留のrunと同じ経路で拾う: review待ちのrunとしてadoptの組み立て直し（ADR-0039）に乗せ、記録済みのpassは`/exit`の後にreviewをやり直さず着地の列に戻る。
   人の`land`の答えで列に並んだrunは、次のprocessがleaseを返し、承認の着地の経路が拾う。
   resumeが解決した（または要らなかった）承認済みのrunは`needs_session`のまま着地slotを待つので、次のprocessは、resumeの後に着地の列に並んだ記録（そのあと着地・review・resumeが始まっていないもの）からそのrunを見分け、resumeをやり直さずに着地slotの待ちに戻す。
   leaseを返せばresumeの候補として拾われ、resumeの回数を使ってsessionを開き直すことになるためである。
   drainが待つのは、要求を受け取った時点で既に始まっていた着地・validation・e2eだけである。
   止めるdrain（`down`・provisioningの失敗）は今どおり着地slotを待つrunを進め、着地の保留の規則（ADR-t3048-1決定2）も変えない。
2. **既に始まっていた着地を待つあいだも、走るworkerのslotのreceiptを観測する。**
   handoffのdrainが既に始まっていた着地を待つあいだ、supervisorは各passで走るworkerのslotのreceiptを観測し、その着地がまだ走っているあいだはvalidationまで進める。
   着地が終わった後に未着手のreceiptはexecを待たせず、次のprocessが観測の記録から続ける。
   実装は後続のtaskが持つ。

ADR-t3048-1は決定1・2が残るので丸ごと置き換えず、決定3の後半（保留の無い着地slot待ちのrunはすぐ着地を始めるので、handoffがその着地を待つ）だけをこのADRの決定1で変える（ADR-t1091-1）。

## Alternatives

- **今どおり保留の無いrunの着地を待つ**: 着地の検証の長さだけexecとreceiptの観測が延び、handoffの上限切れにもなる（Contextの記録）。
- **drainのあいだも組み立て直せるslotを全部進める**: 走るsessionの`/exit`やreviewが始まり、handoffが待つものが増えて区切りが来なくなる。receiptの観測とvalidationまでに限る（決定2）。

## Consequences

- handoffをまたいだ着地slot待ちのrunは、次のprocessでadoptの経路（`/exit`→着地の列）を通り、着地の列に並んだ記録がもう1回残る。着地はexecの数秒とadoptの分だけ遅れる。
- handoffのdrainは、要求の時点で走っていた着地の残りの時間より長く待たない。
