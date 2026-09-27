---
id: adr-t728-2
type: adr
title: 着地（rebase・再検証・squash・mainの更新・push）は信頼するIntegratorだけが行い、supervisorと人のCLIは依頼を出す。reviewのpassは着地の必要条件で、着地の実行ではない
status: accepted
created: 2026-09-27
updated: 2026-09-27
accepted_on: 2026-09-27
owners:
  - hisamekms
tags:
  - runtime
  - security
  - integration
related:
  - adr-0008
  - adr-0027
  - adr-0049
  - adr-t728-1
  - adr-t728-3
---

# ADR-t728-2: 着地（rebase・再検証・squash・mainの更新・push）は信頼するIntegratorだけが行い、supervisorと人のCLIは依頼を出す。reviewのpassは着地の必要条件で、着地の実行ではない

## Context

mainへの着地はdagqで最も影響の大きい外部副作用で、共有のbranchを書き換え、remoteへpushする。今は、supervisorの着地のthreadと人やsessionが打つ`dagq integrate`が同じapplicationの関数を呼んで着地する（[ADR-0008](0008-merge-queue-squash-landing.md)、[ADR-0049](0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)決定1）。一方、呼び出し元の区別は無く、`DAGQ_ROLE`がworkerやplannerのsessionからも`integrate`を打てる。headlessのreviewのpassは、そのままsupervisorが着地の関数を呼ぶ合図になっている（[ADR-0027](0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)）。

[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)は信頼する制御側と信頼しないAI actorを分け、信頼する制御側に**integrator**を足した。このADRはその責務を決める。

## Decision

1. **着地はIntegratorだけが行う。** rebase、rebase後の再検証（検証コマンドの実行）、squash、mainの更新、pushは、信頼する制御側のIntegratorの文脈でだけ実行する。Integratorは決定的なRustのコードで、AI actorではない。
2. **supervisorと人のCLIは依頼を出す。** supervisorの着地のthreadも、人（とinboxの代行。[ADR-t728-3](2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)）が打つ`integrate`も、着地の依頼をIntegratorに渡し、Integratorが前提（runの状態、lease、receipt、要求するevidence、scope、検証の結果）を自分で確かめてから着地する。依頼を出せるかは[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)のpolicyが決め、今の運用で着地を依頼しないworker・job・observer・plannerは依頼を出せない（ADR-t728-1決定7）。
3. **reviewのpassは必要条件で、着地の実行ではない。** review-jobのpassは「着地してよい」というデータで、supervisorがそれを依頼に写す。Integratorは依頼を受けても、自分の検査（receiptを信用しない再検証を含む）を通らなければ着地しない。reviewが人に委ねたconcernの、着地させるという答えも同じく依頼で、着地の実行ではない。
4. **host実行ではIntegratorもsupervisorと同じプロセスとユーザーで動く。** この段のIntegratorの境界は論理的なもので、同じhostの敵対的なプロセスがGitを直接操作することは止められない（ADR-t728-1決定6）。Integratorを別のプロセスや資格情報（pushの鍵）に分けるのは、隔離とqueue serviceを扱う後のgoal（draftのgoal 38）にする。

依頼の型の名前、記録するeventと欄、関数とファイルの名前は[docs/design/](../design/)（[integrate](../design/supervisor-lifecycle/integrate.md)と、goal 55のセキュリティの文書）に書く。

## Alternatives

- **supervisorが今どおり着地の関数を直接呼ぶ**: 着地の前提の確認が呼び出し元ごとに分かれ、どの経路でも同じ検査を通ることを型で保証できない。
- **reviewのpassで着地を確定する**: 信頼しないAI actorの出力が特権の状態遷移を確定することになり、ADR-t728-1の原則に反する。
- **この段でIntegratorを別プロセスにする**: host実行では同じユーザーの権限で動くので強制は増えず、実装の範囲だけが増える。境界を先に型で分け、分離は隔離と一緒に行う。

## Consequences

- 着地とpushの経路が1つになり、誰の依頼で着地したかが記録に残る。
- 後の隔離（Integratorだけがpushの資格情報を持つなど）は、この境界をそのまま強制の点にできる。
- 人のCLIの`integrate`は今と同じように使え、運用は変わらない。
