---
id: adr-t2113-3
type: adr
title: podman（container）を使う実行側はworker・resumeとintegrateのverificationだけにし、読むだけのheadlessのjobはOSのsandboxに留め、e2e・macOSに固有のtest・制御側はhostに置く（ADR-t1233-1決定1をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1233-1 decision 1
amended_by:
  - adr-t2114-3
  - adr-t2114-4
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t2113-1
  - adr-t2113-2
  - adr-t1233-1
  - adr-t1233-2
  - adr-t1233-3
  - adr-t1433-1
  - adr-t827-3
  - design-security
  - design-architecture
---

# ADR-t2113-3: podmanを使う実行側はworker・resumeとintegrateのverificationだけにする

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定1は、制御側（supervisor・runner session・inbox・planner・Gitのpushの権限・queue service）をhostに置き、実行側（worker・resume・headlessのjob・integrateのverification）を隔離環境（まずsandbox、次にコンテナ）に置き、見せるのはworktreeとrun dirだけにした。
例外はe2eで、制御側がhostで流す（ADR-t1233-2）。
cmuxの部分は[ADR-t1433-1](2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)がamendsし、cmuxはinboxだけが使う。

実行側のうち、信頼しないコード（workerが書いたcargoのbuild.rs・proc-macro・test）を走らせるのは、worker・resumeとintegrateのverificationである。
headlessのjob（review・plan review・goal review・triage・復旧のjobなど）は、diff・文書・queueを読んで判断を書くだけで、worktreeのコードをbuildも実行もしない。
macOSのPodmanは同時に1つのmachineしか動かせず、hostは8コア / 16GBで資源が不足しがちで、containerの起動とmachineの資源は実行側の数だけ増える。
今podmanを使っているのはresource brokerの周りだけで、[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)がresource brokerを外す。

## Decision

1. **containerで動かすのはworker・resumeとintegrateのverificationだけ。**
   信頼しないコードを走らせるこの3つを、runごとのcontainer（podman、dagq専用のmachine。ADR-t2113-1決定4〜6）に移す。
   containerに見せるのはrunのworktree（gitは[ADR-t2113-2](2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)のclone）とrun dirだけにする。
2. **読むだけのheadlessのjobの隔離はOSのsandboxにし、containerに移さない。**
   review・plan review・goal review・triage・復旧のjobなど、信頼しないコードを走らせないheadlessのjobは、OSのsandbox（macOSのSeatbelt）で足りる見込みとして、containerに移さない。
   今OSのsandboxで動くのはCodexのjob（読み取り専用）だけで、Claudeのjobはsandboxを持たず（advisory）、今のproviderのsandboxは読み取りと外への通信を絞らない。
   読める範囲をworktreeとrun dirに、外への通信をqueueのbrokerに絞ることは、この段の隔離の実装が足す要件である。
   jobが信頼しないコードを走らせるようになったとき、またはOSのsandboxでこの要件を満たせないと分かったときは、そのjobをcontainerの対象に入れるかをこの決定の見直しとして決める。
3. **hostに置くもの。**
   e2e（実cmuxと実launchdを使う。ADR-t1233-2）、macOSに固有のtest（Linuxのcontainerでは走らない）、制御側（supervisor・queue service・queueのbroker・inbox・planner）はhostに置き、podmanを使わない。
4. **ADR-t1233-1決定1を変える範囲と保つ範囲。**
   変えるのは、実行側の隔離の行き先だけである。
   headlessのjobの隔離はOSのsandboxにし、コンテナに移すのはworker・resumeとintegrateのverificationだけにする（決定1の「まずsandbox、次にコンテナ」はこの3つにだけ当てはめる）。
   保つのは、制御側と実行側を分けること、実行側に見せるのはworktreeとrun dirだけであること（headlessのjobはsandboxでそう絞る。上の決定2の要件）、e2eの例外、cmuxの部分（ADR-t1433-1のまま）である。

## Alternatives

- **headlessのjobもcontainerに移す（ADR-t1233-1決定1のまま）**: jobは数が多く短いので、containerの起動とmachineの資源の費用が隔離の利得を上回る。
  読むだけのjobが走らせうるのはproviderの道具だけで、書き込みと読める範囲と外への通信をOSのsandboxの規則で絞れる見込みがある（絞れなければ決定2の見直し）。
- **workerもOSのsandboxに留める**: cargoのbuildとtestはsandboxの中でも任意のコードを走らせ、macOSのsandboxの規則はprovider・版ごとに違い、hostのファイルとプロセスとの境界が弱い。
  信頼しないコードを走らせるものはcontainerで閉じる。
- **macOSに固有のtestもcontainerで流す**: Linuxのcontainerでは対象のコードがbuildされないので、流せない。

## Consequences

- 今podmanを使うのは、resource brokerのcontainer・image・machine、`up`のpreflight、`down`の停止、machineのgvproxyの片付け、e2eの`broker::`と関門のpodmanの確かめだけである。
  resource brokerを外すと（撤去の実装は別のgoal）、これらはworkerのcontainer化まで要らなくなり、hostはpodmanが無くても動く。
- machineのvolume（既定でhostの`$HOME`をVMにmountする。ADR-t827-3決定6）は、containerに見せるもの（worktreeとrun dir）が見えるだけに絞る方向で、VMの中に置く通信の中継の場所と合わせて後続のqueueのbrokerのADRとworkerのcontainer化の実装が決める。
  compileの共有のcacheをcontainerにmountするなら、決定1の「worktreeとrun dirだけ」の例外として後続のADRで決める。
- headlessのjobの隔離はOSのsandboxの強さに依存し、Claudeのjobにsandboxを付けることと、読み取りと外への通信を絞ることが実装に残る。
  `status`と`doctor`のenforcementは、containerとsandboxで違う値を出す必要がある。
- macOSに固有のtestはintegrateのcontainerのverificationに含まれないので、hostかCIで流す場所を実装のgoalが決める。
