---
id: adr-t2424-1
type: adr
title: containerのrunの着地の再確認の`[recheck] command`はintegrateのverificationのcontainerで流し、流せないときはhostで流さず何も判定しない（ADR-t2113-3決定1をamends）
status: accepted
created: 2026-10-17
updated: 2026-10-17
accepted_on: 2026-10-17
amends:
  - adr-t2113-3 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - security
related:
  - adr-t2113-3
  - adr-t2113-1
  - adr-t2179-1
  - adr-t2114-1
  - adr-t2114-4
  - adr-t2182-1
  - adr-0068
  - adr-t639-1
---

# ADR-t2424-1: containerのrunの着地の再確認のcommandはverificationのcontainerで流す

## Context

[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1は、信頼しないコード（workerが書いたbuild.rs・proc-macro・test）を走らせるworker・resumeとintegrateのverificationをrunごとのcontainerに移し、決定3はe2e・macOSに固有のtest・制御側だけをhostに置いた。
着地の再確認（[ADR-0068](0068-recheck-waiting-runs-after-each-landing.md)）は、着地を待つrunをmainに載せた木で`dagq.toml`の`[recheck] command`を流し、意味の衝突を着地の前に見つける。
gitの確かめは[ADR-t2179-1](2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)決定8で制御側がguardして流すが、commandはrunの木をcompileするので、containerのrunでもworkerのコードをhostで走らせていた。
このcommandはどの決定3の例外にも当たらず、性質はintegrateのverificationと同じである。

## Decision

1. **containerのrunの`[recheck] command`は、integrateのverificationのcontainerで流す。**
   ADR-t2113-3決定1の「integrateのverification」に、workerがcontainerで動くrunの着地の再確認のcommandを含める。
   integrateのverificationのruntimeがcontainerのとき、そのcontainerと同じ形（mainにrunを載せたcommitの制御側が作るclone・ネットワークから切り離し、外への経路はverificationのtokenで`dagq.toml`のregistryだけ・workerのcacheもhostのcacheもmountしない）で流す。
   tokenは同じrunのintegrateのverificationのものと別に持ち、同時に走ってもお互いを失効させない。
2. **流せないときはhostで流さず、何も判定しない（fail closed）。**
   verificationのruntimeがhostのままのとき、containerが使えないとき（machine・image・brokerなど）、containerを用意できないとき、podmanがcontainerを作れず起動しなかったときは、commandをhostにもどこにも流さない。
   podmanが起動しなかったことはcommandの失敗と分け、commandが流れなかったものとして扱う。
   hostでの失敗（[ADR-t639-1](2026-09-27-t639-1-retry-verification-failures-of-the-host-once.md)）と同じく何も判定せず、runは着地待ちのまま、resumeにもholdにもせず、理由をeventとrunの開いたaskに残す。
3. **変えないもの。**
   gitの`merge-tree`の確かめは今どおり制御側がguardして流す。
   hostのrunの着地の再確認（scratchのworktreeと1つのtargetのdirで流す形）は変えない。

## Alternatives

- **着地の再確認をADR-t2113-3決定3の例外としてhostに残す**: 信頼しないコードをhostで走らせないというgoalの目的と食い違う。
- **containerのrunではcommandを飛ばし`merge-tree`だけを見る**: 意味の衝突を着地の前に見つけられず、着地の検証で落ちてからresumeになる。
- **workerのcontainer（cacheをmountする）で流す**: workerが書いたcacheでcompileすることになり、integrateのverificationがworkerのcacheを使わない理由（[ADR-t2114-4](2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)決定4）がそのまま当てはまる。

## Consequences

- containerのrunの再確認はcacheなしでcompileするので、hostのrunの再確認より時間がかかる。
- verificationのruntimeをhostにしたままworkerだけをcontainerにした`dagq.toml`では、containerのrunの再確認はcommandを流さず、`merge-tree`だけが意味を持つ。
- podman自身の失敗を表す終了コードは、containerの中のcommandも同じコードで終われば見分けられず、そのcommandは流れなかったものとして何も判定しない。
- integrateのverificationがpodmanの起動の失敗をcommandの失敗と分けるかは、この決定の範囲の外である。
