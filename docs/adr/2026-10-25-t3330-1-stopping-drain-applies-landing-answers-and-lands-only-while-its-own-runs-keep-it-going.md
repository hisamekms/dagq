---
id: adr-t3330-1
type: adr
title: 停止要求のdrainも答えの済んだapprove_landingを毎pass適用し、承認された着地は自分のrunがdrainを続けるあいだだけ始める
status: accepted
created: 2026-10-25
updated: 2026-10-25
accepted_on: 2026-10-25
owners:
  - hisamekms
tags:
  - runtime
related:
  - adr-0071
  - adr-t3077-1
  - adr-t3049-1
  - design-supervisor-lifecycle-supervise
  - design-supervisor-lifecycle-up-down
---

# ADR-t3330-1: 停止要求のdrainも答えの済んだapprove_landingを毎pass適用し、承認された着地は自分のrunがdrainを続けるあいだだけ始める

## Context

停止要求（SIGINT/SIGTERMの1回目・runの終わりを待つ停止・supervisorの入れ替えのdrain）で drain に入ったsupervisorは、新しい仕事を始めないためにfill passを呼ばない。
leaseを持たない`awaiting_integration`のrunの`approve_landing`の答え（`land`・`send_back`・`cancel`・`retry_review`）の適用もfill passの中にあったので、drainのあいだは止まっていた。
drainは[ADR-0071](0071-runs-waiting-in-revise-and-resume-leave-the-slot.md)決定11のとおり人の答えを待つrunの終わりも待つので、長いdrainのあいだ答えの済んだ着地と差し戻しが次のsupervisorの起動まで止まる。
2026-10-12T23:01Zからのdrainは、worker_questionの答えを待つrun 4件のため約13時間続き、そのあいだに答えの済んだ5件の着地の判断が適用されなかった。

## Decision

1. **停止要求のdrainも、毎passで答えの済んだ`approve_landing`を適用する。**
   leaseを持たないrunのaskを、drainでないpassと同じ規則で適用して閉じる（`land`は承認を記録して着地の列に入れ、`send_back`は`needs_session`にし、`cancel`はrunを失敗にしてtaskをcancelし、`retry_review`はreviewのやり直しの列に入れる）。
   handoffのdrain（[ADR-t3077-1](2026-10-22-t3077-1-handoff-drain-starts-no-landing-and-hands-over-runs-awaiting-the-slot.md)）は対象にせず、今のとおり答えを次のprocessに残す。
2. **承認された着地は、自分のleaseのrunがほかに残りdrainが続くあいだだけ始める。**
   `land`で列に入ったrunの着地は、このdrainがそうして始めた着地のほかに自分のrunが残っているときだけ始め、そのような着地は以後drainが待つ自分の着地になる。
   残っていなければ始めずにdrainを終え、次のsupervisorが列の記録からそのrunを拾ってreviewをやり直さずに着地させる。
   自分のrunが着地slotを待つあいだは、そのrunの着地を先にするため始めない。
   答えの適用でdrainの終わりを延ばさないためで、自分のrunの着地を進める今の停止のdrainと揃う。
   始めるのは`land`の答えで列に入ったrunだけで、死んだ着地の回復で列に戻ったrunは今のとおり次のsupervisorに残す。
   ownerの居なくなったsupervisorは猶予のうちに終わるので始めない。
3. **drainは新しい仕事を始めない（今のとおり）。**
   claim・resume・reviewのやり直し・`already_satisfied`のclaimの答えの適用（検証のやり直し）・leaseも答えも無い着地待ちのrunの拾い直し（[ADR-t3049-1](2026-10-21-t3049-1-supervisor-takes-up-a-passed-run-left-awaiting-integration-without-lease-or-ask.md)）は始めず、次のsupervisorに残す。
   `send_back`の後のresumeと`retry_review`のreviewも、次のsupervisorが始める。

ADR-0071決定8・決定11（drainは人の答えを待つrunも待ち、待ちの終わったrunはdrain中もslotに戻す）とそのAlternatives、ADR-t3077-1の決定は変えない。

## Alternatives

- **drainを人の答えで打ち切る・待ちのrunのleaseを残して終える**: ADR-0071決定11のAlternativesが、runの終わりを待つ停止の「走っているものが終わった」という意味（非互換のmigrationの前提）を理由に退けている。採らない。
- **drainで承認された着地をいつも始める**: 自分のrunが残っていないdrainは、答えの適用で着地の検証の長さだけ延びる。停止を頼んだ人の待ちを答えで延ばさない。
- **drainで答えを適用せず、今どおり次のsupervisorに任せる**: Contextの13時間の止まりが残る。

## Consequences

- 長いdrainのあいだも、人の`land`は自分のrunが残るあいだに着地し、`send_back`・`cancel`・`retry_review`は記録され、次のsupervisorは記録された列から続ける。
- 自分のrunが残らないdrainで`land`の答えを受けたrunは、次のsupervisorの起動まで着地を待つ。
