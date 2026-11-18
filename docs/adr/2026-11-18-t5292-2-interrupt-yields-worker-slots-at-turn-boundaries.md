---
id: adr-t5292-2
type: adr
title: interruptの候補へsupervisor自身のholdでturnの終わりにslotを譲る
status: accepted
created: 2026-11-18
updated: 2026-11-18
accepted_on: 2026-11-18
amends:
  - adr-0049 decision 4
  - adr-t1850-1 decision 6
  - adr-t1879-1 decision 3
  - adr-t1879-1 decision 5
owners:
  - hisamekms
tags:
  - architecture
  - supervisor
  - priority
related:
  - adr-t5292-1
  - adr-t1850-1
---

# ADR-t5292-2: interruptの候補へsupervisor自身のholdでturnの終わりにslotを譲る

## Context

効く優先度を1本の列で比べても、workerのslotが全て埋まっていればinterruptの候補は待つ。
人はgoal 319で、低い優先度のrunに安全な境界でslotを譲らせ、interruptのclaimの後に自動で戻すことを選んだ。
[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4は、interruptをclaimの順に効かせ、走っているrunを割り込みで止めないとした。
[ADR-t1850-1](2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)決定6は、再開と復旧jobを含む列にもその方針を引き継いだ。
turnの終わりまで待つ譲りでも、優先度を理由に低い優先度のrunの次の工程を保留するため、両決定の「走っているrunを割り込みで止めない」部分を改める。
[ADR-t1879-1](2026-10-07-t1879-1-hold-a-task-at-the-turn-boundary-and-release-it-explicitly.md)決定2の安全な境界を使い、決定3・5のholdと解除を人とinboxに限る部分に、supervisor自身の譲りを加える。

## Decision

1. **譲りはAdmissionの上の規則として判断する。**
   slotを渡す列の先頭の候補の効く優先度がinterruptで、workerのslotに空きが無く、その候補のための譲りがまだ掛かっていないときだけ譲りを求める。
   返る途中の譲りは返るslotとして数え、同じ候補のために追加で譲らせない。
   urgent以下では譲らせない。
2. **譲るのは、workerのslotに数えるrunのうち、候補より効く優先度が低く、workerのturnの中かturnの間にいる1本だけとする。**
   着地の検証・reviewのjob・e2e・着地の列・人の待ちにいるrunは対象にしない。
   taskに既にholdがあれば、誰が掛けたものでも対象にしない。
   対象の中で効く優先度が最も低いrunを選び、同じならclaimが最も新しいrunを選ぶ。
   対象が無ければ何もしない。
3. **supervisorは譲りのためだけに、自分のものと分かるtaskのholdを掛けられる。**
   人のholdと同じ止め方とbuildの片付けで、低い優先度のrunにturnの終わりでslotを返させる。
   このinterruptの譲りに限って、ADR-0049決定4とADR-t1850-1決定6の、走っているrunを割り込みで止めない規則を改める。
   走っているturnを途中で殺さず、validation・review・e2e・着地も途中で殺さない。
   人やinboxのholdを譲りのholdとして引き取ったり変更したりしない。
4. **supervisorは自分の譲りのholdだけを、同じrunを続ける解除で外せる。**
   外すのは、slotが返りinterruptの候補がclaimされた後か、候補が列の先頭でなくなった後の回とする。
   解除には既存の安全な待ちの境界を守り、返る途中で次のturnを始めない。
   外したrunはsessionを待つrunとして共通の列に戻り、効く優先度の順に再開する。
   人のreleaseは要らず、人・inboxのholdは外さない。
5. **譲りの始まりと終わりを記録に残す。**
   statusのholdの一覧には譲りと分かる理由を出す。
   譲りのholdは長いholdのattentionとwatchの知らせから除く。
   人の判断で待たせているholdと、自動で返すslotの譲りを混同させない。

## Alternatives

- **goal 318の小さい直しとして戻り待ちの関門だけを変える、または譲りだけを足す**: 関門が散らばったままでは、2026-11-18の5283が38分claimされなかった食い違いと行き詰まりが同じ形で繰り返される。
  譲りも統合したAdmissionの上の規則にする必要がある。
- **diskのLandingFirstだけを直す**: slotが埋まったときのinterruptの待ちは解けず、開始と表示の別々の関門も残る。
- **割り込みを引き続きclaimの順だけに効かせる**: ADR-0049決定4とADR-t1850-1決定6では、途中の作業の退避ややり直しを避けるため、slotが自然に空くのを待つ。
  そのままでは次のturnへ進むrunがslotを使い続け、interruptが列の先頭でも始められない。
  既存のholdの境界で成果を残し、同じrunを再開できるため、interruptに限ってturnの終わりで譲る案を採る。
- **turnを途中で殺す**: 途中の成果を失い、安全な境界で止める不変条件を破る。
- **urgent以下でも譲る**: 通常の優先度の入れ替わりまで走りかけの作業を止め、譲りと再開を増やす。
- **人にreleaseを求める、または人のholdも自動で外す**: 前者は一時的な譲りを人の作業にし、後者は人が止めた理由をsupervisorが代わりに解消したとみなしてしまう。

## Consequences

interruptもturnが終わるまでは待つが、低い優先度のrunがslotを占め続けるためには待たない。
人のholdの権限は保ち、supervisorの権限の追加を譲りの開始と解除だけに限定する。
実装は所有の区別・安全な境界・列への再投入・表示を揃え、designとpluginの案内も振る舞いと一緒に更新する。
