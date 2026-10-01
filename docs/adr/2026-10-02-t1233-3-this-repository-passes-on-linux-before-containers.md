---
id: adr-t1233-3
type: adr
title: このrepository（dagq自身）は、先にLinuxでbuildとtestを通し（CIにLinuxを足し、macOSに固有のtestを分ける）、その後に自分のrunの実行側をコンテナで動かす
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
owners:
  - hisamekms
tags:
  - testing
  - ci
  - security
related:
  - adr-t1233-1
  - adr-t1233-2
  - adr-t728-1
  - adr-t827-3
---

# ADR-t1233-3: このrepositoryは先にLinuxでbuildとtestを通し、その後にコンテナで動かす

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)は実行側（worker・resume・headlessのjob・integrateのverification）を隔離環境に置き、sandboxの次にコンテナにすると決めた。コンテナの中はLinuxである。このrepositoryのCI（`.github/workflows`のci・release・stress）は今`macos-14`だけで動き、Linuxでbuildとtestが通る保証が無い。dagqの開発自体もdagqで流す（ドッグフーディング）ので、このrepositoryのworkerをコンテナに移すと、workerの手元のtestと`integrate`のverificationがLinuxで走る。

goal 38はこれを未決にしていた: Linuxのtestを通すgoalを先に立てるか、このrepositoryはsandboxまでにとどめてコンテナは他のrepository向けにするか。2026-10-01に人は前者を選んだ。

## Decision

1. **このrepositoryは、先にLinuxでbuildとtestを通す。** CIにLinuxのjobを足し、`cargo build`・fmt・clippy・coverageの関門と同じ全体のtestをLinuxで通す。macOSにしか無いもの（launchd、macOSのsandbox、実cmuxなど）に依るtestは、OSで分けてLinuxでは流さないか別の実装で確かめ、黙って落ちたまま残さない。macOSのCIは残す（制御側と人の環境はmacOSのまま）。
2. **このrepositoryの実行側をコンテナで動かすのは、Linuxのbuildとtestが通ってから。** それまでは、このrepositoryのrunはhost構成（とsandbox）のまま、goal 82の段(2)〜(3)（queue serviceとクライアントモード。host構成で着地できる）を進める。コンテナでの実行（goal 38の段(4)以降）はこのrepositoryではLinuxのgoalの後にする。
3. **e2eはLinuxの対象に含めない。** 実cmuxを使うe2eはhostの制御側の工程で流す（[ADR-t1233-2](2026-10-02-t1233-2-e2e-runs-on-the-host-after-review-passes.md)）ので、Linuxで通すのはe2eを除くtestである。

どのtestをどう分けるか、CIのrunnerとarchitecture、Linuxのjobの名前はLinuxでbuildとtestを通すgoalのtaskが決めて`docs/design/`とworkflowに書く。

## Alternatives

- **このrepositoryはsandboxまでにとどめ、コンテナは他のrepository向けにする**: dagq自身が自分の隔離を使わず、ドッグフーディングで隔離の不具合を見つけられない。人はLinuxを先に通すと決めた。
- **コンテナに移してからLinuxの失敗を直す**: 本番のrunがOSの違いで落ち、resumeと着地の失敗が隔離の不具合と混ざって切り分けにくい。
- **macOSのコンテナ（VM）で動かす**: 一般的なコンテナのruntimeはLinuxで、macOSのゲストは運用の重さと資源の制約が大きい。

## Consequences

- コンテナ化の前にLinuxのgoal（CIのjobの追加とOSに固有のtestの分離）が入り、goal 38の段(4)以降はその後になる。
- LinuxのCIの分、CIの時間が増える。OSで分けたtestは、どちらのOSで何を確かめているかを文書に残す必要がある。
- 他のrepositoryでのコンテナの利用は、このrepositoryのLinuxの対応を待たない。
