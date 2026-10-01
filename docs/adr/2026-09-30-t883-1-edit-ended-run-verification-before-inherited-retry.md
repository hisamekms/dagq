---
id: adr-t883-1
type: adr
title: 終了したrunのverifyだけを人とinboxが直して作業を引き継ぐ
status: accepted
created: 2026-09-30
updated: 2026-10-02
accepted_on: 2026-09-30
amends:
  - adr-0047 decision 9
  - adr-0047 decision 39
  - adr-t728-1 decision 7
related:
  - adr-t728-3
owners:
  - hisamekms
tags:
  - runtime
  - recovery
  - authorization
---

# ADR-t883-1: 終了したrunのverifyだけを人とinboxが直して作業を引き継ぐ

## Context

Task 572 のrun 406baa0dでは成果のcommitがあったが、verifyの`python3`がhostの3.9.6で`tomllib`を使えず着地に失敗した。ask 154/159/160で人はverify修正後の`retry_inherit`を求めたが、`edit`はdraft/submitted限定で、成果を捨てる`retry`を経由するしかなかった。

## Decision

1. `in_progress`で最新のrunが`failed`または`interrupted`に終わり、unfinished run（`claimed`・`starting`・`running`・`validating`・`awaiting_integration`・`integrating`・`needs_session`）が一つも無いtaskでは、userとinboxの`edit --verify` / `--no-verify`だけを許す。判定と変更は同じDB transactionに置く。`required_evidence`と`paths`は含めない。これらは検証手順の誤記よりも成果の要件・作業範囲を変えるため、従来の計画経路を通す。
2. `task.verify_edit` capabilityをuserとinboxにだけ与える。planner・worker・全jobは拒む。通常の`task.write`は広げない。`task_edited`は変更前後の`verification_commands`とactorを残す。
3. 復旧jobのdecide askは、runの`integrate`の検証が落ちたことがある（verifyが原因でありうる）ときだけ、人またはinboxが先にeditし、その後「edit the task's --verify, then retry_inherit」を選ぶ道を示す。この回答はjobの次のroundへ戻す。jobは現在のverifyと`task_edited`を読み、修正済みなら`retry_inherit`を選べる。次のrunのintegrateはtaskから現在のverifyを読む。
4. runの開始より後にverifyを直す`task_edited`があるこの回答は、ADR-0047決定39のalertごとの3回を使い切った後でも、jobのroundをもう1回持つ。使い切った後のaskでも同じoptionを示す。editの無い回答は他のoptionと同じに数え、使い切っていればaskがまた開く。回数の延長はverifyを直した後に人がこのoptionで答えるたびに1回だけなので、runtimeだけで回り続けることはない。

## Alternatives

- **復旧jobのverdictがverifyの修正を提案し、人の承認でruntimeが適用する**: 採らない。復旧jobは状態を変えるコマンドを打てず（goal 55）、verifyの中身は人がaskの答えに書いた文から決まるので、inboxが人の言葉で`edit`してから答える方が経路が短く、書き換える権限をjobに広げずに済む。
- **`required_evidence`と`paths`も同じ例外で直せるようにする**: 採らない。どちらも検証の手順の誤記ではなく、成果に求める要件と作業の範囲を変えるので、plan reviewを通る計画の経路に残す。
- **verifyのoptionの回答でruntimeが自分で`retry_inherit`を適用する**: 採らない。jobの判断（branchの中身・失敗の原因の読み）を飛ばすうえ、`retry_inherit`の前提の検査と記録をjobのverdictの経路と二重に持つことになる。

## Consequences

ADR-0047決定9の編集可能期間に、この限定的な例外を足す。ADR-0047決定39の3回の上限に、人がverifyを直したときだけの1回の延長を足す。ADR-t728-1決定7のroleの権限表をこの操作だけ広げる。ADR-t728-3のinboxによる代行とactorの区別はそのまま使う。生きているrunの検証内容は変更されない。
