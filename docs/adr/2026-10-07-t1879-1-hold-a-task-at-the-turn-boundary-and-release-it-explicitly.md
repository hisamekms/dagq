---
id: adr-t1879-1
type: adr
title: 人とinboxは理由を付けてtaskをhold（保留）でき、runtimeは走っている工程を止めずにその後の進行・claim・復旧job・resume・着地と、holdの前に集めた候補・結果・回答の適用を止め、成果とsessionを残して、人の明示の解除で続け方とprovider・modelを選ばせる
status: accepted
created: 2026-10-07
updated: 2026-10-07
accepted_on: 2026-10-07
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
  - hold
related:
  - adr-0049
  - adr-t1850-1
  - adr-t1521-2
  - adr-t1662-1
  - adr-t1662-2
  - design-supervisor-lifecycle-task-hold
---

# ADR-t1879-1: taskをturnの境界でholdし、人の明示の解除で続け方とprovider・modelを選んで再開する

## Context

runの妨げ（環境・認証・外のサービスの都合）を取り除く間、taskを待たせたいことがある。
今は、生きているrunを人が止める手が無い。
runへの送信は拒まれ、recoverはprocessが生きていれば拒み、cancelとdraftに戻すことは終わっていないrunを持つtaskでは拒まれる。
止められるのは、止まったrunのaskに止めると答える経路だけで、そのときrunは失敗になる。
resumeを待つrunのproviderとmodelを変える手も無い。
taskをdraftに戻せても、終わったrunの復旧jobは人を待たずにやり直しやresumeを適用するので、人の操作と競合する。

[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定4（と[ADR-t1850-1](2026-10-06-t1850-1-resumes-recovery-jobs-and-claims-share-one-line-by-effective-priority.md)決定6）は、走っているrunを割り込みで止めない（preemptionしない）と決めている。
[ADR-t1521-2](2026-10-05-t1521-2-trusted-runtime-parks-snapshots-and-atomically-replaces-tasks.md)決定1は、長期化したrunを再計画するために、trusted runtimeが安全な境界でrunを保留し、次のturn・続行の配送・着地を止めると決めている。
holdは再計画と違って計画を変えず、計画は正しいまま環境の都合で待たせる。
draftは「計画が固まっていない」を言い、plan reviewを通った扱いを捨てるので、holdには使えない。

## Decision

1. **holdの対象はtaskで、draftと別の印にする。**
   runを名指されたら、そのrunのtaskのholdとして受ける。
   runは解除の後に作り直されることがあるので、runに印を置かない。
   holdはtaskのstatusを変えず、plan reviewを通った扱い（ready・submittedを経たこと）を失わせない。
   draft・cancel・recover・runへの送信の意味は変えない。
2. **走っている工程を止めない（非preemption。ADR-0049決定4）。**
   holdの時に走っているturn・validation・review（と着地の前のe2e・着地）は途中で殺さず終わりまで行かせ、その後の工程（次のturn・差し戻し・resume・回答の配送・着地の開始）へ進めない。
   holdの待ちは止まり（stall・idle）と数えず、runを失敗にしない。
3. **かけるのも外すのも人とinboxだけで、理由を必須にする。**
   worker・planner・jobとsupervisorはholdもその解除もしない。
4. **holdの間も成果を捨てない。**
   sessionと成果のbranch・worktreeの未commitの差分を、掃除の対象にしない。
5. **解除は人の明示の操作だけで、自動では外さない。**
   かけたまま忘れないよう、長く続くholdを人に知らせる。
   解除のときに、続け方（同じsessionで続けるか、成果を引き継いでrunを作り直すか）とprovider・modelを選べる。
   解除はrunが安全な待ちに入った後だけ受け、走っている工程が残る間の解除は理由を付けて拒み、holdも選んだ値も変えない。
6. **holdは開始と結果の適用に先立つ。**
   supervisorは、holdの前に集めた候補（claim・resume・復旧job）を始める直前にholdを確かめ直し、hold中のtaskを始めない。
   holdの前に起動した復旧jobの結果と、人の回答は、hold中のtaskを進める形で適用せず記録として残す。
   解除の後に、taskとrunの版が変わっていなければ適用し、変わっていれば古いものとして捨て、どちらにしたかを記録する。
   holdを確定する記録と状態は1つの確定で書き、安全な待ちへ入ることと解除は、それぞれ相手の状態を同じ確定の中で確かめる。
7. **turnの境界の保留はADR-t1521-2決定1の再計画の保留と1つの機構にする。**
   次の工程を始める箇所が見る柵を1つにし、holdはその柵に理由を1つ足す。
   先に実装された側の機構を使い、二重に作らない。
8. **範囲を絞ったholdは今は作らない。**
   queue全体・provider・areaの範囲のholdは、今あるclaimの控えとqueueのholdの仕組みに乗せる別の層として後から足す。

## Alternatives

- **走っているrunを止める（kill）**: 作業の途中の状態を捨てるか退避する仕組みが要り、ADR-0049決定4に反する。
- **draftに戻して待たせる**: plan reviewを通った扱いを失い、終わっていないrunを持つtaskでは使えず、復旧jobとの競合も残る。
- **runに印を置く**: 解除で作り直したrunに印が残らず、readyのtaskを待たせられない。
- **期限で自動に外す**: 外の都合が片付いたかは人しか分からないので、期限は知らせるだけにする。
- **holdのための別の境界の止め方を作る**: 再計画の保留と同じ柵を二重に持ち、片方だけが見る経路が漏れる。
- **queue全体・provider・areaのholdを今入れる**: 今の要求はtask単位で、範囲のholdは既存の控えとの関係を別に決める必要がある。

## Consequences

- 状態ごとの効果、開始と結果の適用を止める境界、解除の分岐、CLI・event・既定の時間・権限・計測の区間は[task hold](../design/supervisor-lifecycle/task-hold.md)が持つ。
  実装は後続のtaskが行う。
- 計測は、holdをかけた時刻と、走っていた工程が終わって実際に待ちに入った時刻を分け、後者からの区間を人の待ちとして分析から除けるようにする（[ADR-t1662-1](2026-10-04-t1662-1-runs-are-covering-phase-intervals-folded-into-a-ledger.md)）。
- 権限の表に人とinboxだけのcapabilityが1つ増え、pluginの権限の表と案内も同じ変更で直す。
- 解除で作り直すrunは人の判断なので、復旧jobの自動のやり直しの制限（成果を引き継ぐやり直しをtaskごとに1回にすること）を使わず、数えもしない。
