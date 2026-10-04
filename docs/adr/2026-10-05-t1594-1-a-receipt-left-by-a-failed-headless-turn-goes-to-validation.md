---
id: adr-t1594-1
type: adr
title: 非対話のworkerのturnがreceiptを書いた後に非0で終わったら、runはreceiptをvalidationにかけ、turnの失敗は記録に残す。receiptの無い失敗のturnはADR-t813-1決定9のまま（ADR-t813-1決定9をamends）
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-t813-1 decision 9
owners:
  - hisamekms
tags:
  - runtime
  - supervisor
related:
  - adr-t813-1
  - adr-t1433-2
  - adr-t1410-1
  - adr-0047
  - design-supervisor-lifecycle-headless-worker
---

# ADR-t1594-1: 非対話のworkerのturnがreceiptを書いた後に非0で終わったら、runはreceiptをvalidationにかけ、turnの失敗は記録に残す。receiptの無い失敗のturnはADR-t813-1決定9のまま（ADR-t813-1決定9をamends）

## Context

非対話のworkerのturnがcommitとreceipt（HEAD）を書いた後に非0で終わると（stubのscriptでは「commit; receipt HEAD; exit 7」）、session wrapperはturnを`turn_finished`（`outcome: failed`）とidleの印に記録して1で終わる。supervisorはidleの印をreceiptの後に見ればsessionを開いたままvalidatingにし（`supervision_finished`の`session_live: true`）、wrapperの終了を先に見ればrunを`failed`（`session exited with code 1`）にしていた。同じreceiptと終わり方で、着地とfailedが観測の順で入れ替わる（task 1435のrun ae4bbfa9がfull gateの下で`runtime_sweep`と`runtime_triage`の2本で観測した）。

[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md)決定9は、turnの失敗（非0の終了、失敗の結果）を今の`failed` / `interrupted`と同じく復旧jobにかけるとするが、そのturnがreceiptを残したときを決めていない。非対話のturnの非0は、作業の後の失敗（最後の応答でのAPIの失敗など）でも起きる。対話の経路は[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)で廃止され、workerのsessionは非対話だけになった。

## Decision

1. **receiptを残した失敗のturnはvalidationにかける（ADR-t813-1決定9をamends）。** 最後のturnが非0で終わっても（wrapperが非0で終わっても）、runのreceiptが書かれていれば、runは`failed`にせず`validating`に進み、receiptを今のvalidationにかける。commitがbranchのHEADであること・worktreeがcleanであること・evidenceとscopeの検査はそのまま守る。validationがreceiptを拒めば、今のとおり`failed`（または`needs_session`）になり、`failed`は復旧job（triage）に進む。終了コードの種類は問わず、receiptの後にsessionがsignalで終わったとき（supervisorが止めたsessionの128以上の終了を含む）も同じにする。
2. **receiptの無い失敗のturnは今のまま。** wrapperが非0で終わりreceiptが無ければ、runは`failed`になり`last_error`に`session exited with code N`を書き、復旧jobにかける（ADR-t813-1決定9のまま）。
3. **観測の順に依らない判定。** supervisorはwrapperの終了を見たとき、その時点でreceiptの有無を読んでから決める。判定はsrcの副作用のない関数（終了コードとreceiptの有無から次の状態）にし、unit testで組を確かめる（[ADR-t1410-1](2026-10-03-t1410-1-decisions-in-unit-tests-boundaries-in-integration-tests.md)）。idleの印を先に見た経路（sessionを開いたままvalidating）と結果が揃う。
4. **記録。** turnの失敗はturnの記録に残す。receiptでvalidatingに進んだrunのsessionの終わりの記録は、wrapperの終了コードとreceiptで進んだことを持ち、失敗の理由は付けない。`last_error`は書かない（runは失敗していない。着地したrunに古いerrorを残さない）。eventの欄は[domain-model](../design/domain-model.md)の「集約: TaskRun」が持つ。
5. **「そのturnが書いたreceipt」の読み方。** sessionの段のrunのrun dirのreceiptは、そのrunのsessionが書いたものだけなので、wrapperの終了の時点でreceiptがあればそれをこの決定のreceiptとする。古いreceipt（HEADより前のcommit）はvalidationが拒む。resumeの段はすでに終了コードによらずreceiptで判定しており、変えない。

## Alternatives

- **今のまま非0なら`failed`にし、復旧jobにreceiptを見させる**: 検査を通るreceiptを捨て、復旧jobの`retry`で作業をやり直すか人が要る。観測の順で結果が変わる欠けも残る。採らない。
- **非0のturnの後もidleの印を待ってから決める**: wrapperが終わった後は印を待つ理由が無く、印が見えない場合（書けなかった・supervisorの再起動）に決まらない。receiptの有無で決めれば足りる。
- **対話の経路と揃える**: 対話の経路はADR-t1433-2で廃止されたので、揃える対象が無い。

## Consequences

- 「commit; receipt; 非0の終了」のturnは、supervisorがreceiptとwrapperの終了をどの順で見てもvalidationに進み、検査とreviewを通れば着地する。
- turnの失敗はrunの`failed`ではなくturnとsessionの終わりの記録に残るので、`stats`などでrunの失敗として数えられない。着地の品質はvalidation・review・integrateのverificationが守る。
- 状態の遷移・`last_error`・`supervision_finished`の記録は[domain-model](../design/domain-model.md)の「集約: TaskRun」、supervisorの手順は[supervise](../design/supervisor-lifecycle/supervise.md)の手順8、turnの終わり方は[headless-worker](../design/supervisor-lifecycle/headless-worker.md)が持つ。
