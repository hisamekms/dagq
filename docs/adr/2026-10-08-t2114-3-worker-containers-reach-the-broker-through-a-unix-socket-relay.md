---
id: adr-t2114-3
type: adr
title: workerのcontainerは--network noneにし、VMの中のrelayからrunごとのunix socketをmountし、containerの中で127.0.0.1をsocketに転送してhostのqueueのbrokerに届く（ADR-t1233-1決定6・8、ADR-t2113-3決定1をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1233-1 decision 6
  - adr-t1233-1 decision 8
  - adr-t2113-3 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t1233-1
  - adr-t2113-1
  - adr-t2113-3
  - adr-t827-3
  - adr-t2114-1
  - adr-t2114-2
  - adr-t2114-4
  - design-security
---

# ADR-t2114-3: workerのcontainerは--network noneにし、unix socketのrelayでbrokerに届く

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定6は、containerからqueueのbrokerへloopbackのTCPとtokenで届くと決めた（macOSのcontainerではunix socketのbind mountが安定しないため）。
queueのbrokerはhostのプロセスで、宛先を3つに限る（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）。
containerに普通のnetworkを持たせると、brokerを通らずに外へ出る経路が残り、宛先の制限をcontainerの側にも重ねて持つことになる。
macOSのPodmanのcontainerはVMの中で動き、hostのportへはgvproxyを通って届く。
hostのunix socketをVMの外からcontainerへ渡すのは不安定だが、VMの中で作ったsocketはcontainerにbind mountできる。

## Decision

1. **workerのcontainerは`--network none`にする。**
   worker・resumeとintegrateのverificationのcontainer（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）はnetworkを持たず、loopbackだけが在る。
   外への経路はbrokerへの1本だけにし、宛先の制限はbroker 1か所で持つ。
2. **VMの中のrelayからrunごとのunix socketをmountする。**
   supervisorはcontainerを起動する前に、VMの中にそのrunのunix socketを作り、relayがそれを受けてhostのbrokerのportへ転送する。
   relayは秘密を持たず、認証も宛先の判定もしない（run tokenの検証と宛先の制限はbrokerが行う）。
   socketはそのrunのcontainerにだけmountし、runの終わりに消す。
3. **containerの中で127.0.0.1をsocketに転送する。**
   containerの中に、127.0.0.1のportを受けてmountしたsocketへ転送する小さな転送を置く。
   Claude Codeの`ANTHROPIC_BASE_URL`・cargoのproxy・dagqのクライアントモードの宛先は、この127.0.0.1のportを指す。
4. **懸念と手当て。**
   - SELinuxのラベル: socketとmountには`:z`（共有のラベル）を付け、Fedora CoreOSのVMでcontainerから開けるようにする。
   - uidとsocketの権限: socketはcontainerの非rootのuid（[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)決定6）だけが開ける所有者と0600にし、他のrunのcontainerからは見えない（mountしない）。
   - relayとgvproxyは単一障害点になる: relayのhealthはsupervisorが確かめ、落ちていれば起動し直し、起動し直せなければ新しいcontainerのrunを控えてattentionで人に知らせる。
     gvproxyはmachineの一部で、machineの起動し直しで直す（ADR-t2113-1決定5のlockの中）。
   - machineの`$HOME`のmount（ADR-t827-3決定6の見直し）: dagq専用のmachineはhostの`$HOME`を既定でmountせず、containerに見せるもの（runのworktreeかclone・run dir・[ADR-t2114-4](2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)のworkerのcache）の親のdirectoryだけをvolumeにしてinitする。
     volumeはinitでしか決められないので、範囲を変えるときはmachineを作り直す。
   - DNSが無い: `--network none`では名前を引けず、proxyの環境変数を見ない道具は外へ出られずに落ちる。
     外へ出る道具はproxyを見るもの（cargo・Claude Code・dagq）に限り、ほかの道具の失敗は隔離の結果として扱い、要るものはhostのrunかqueue serviceのユースケースに回す。
5. **ADR-t1233-1決定6を変える範囲。**
   「コンテナからはloopbackのTCPとtokenで使う」を、上の形（containerの中のloopbackの転送 → runのunix socket → VMの中のrelay → hostのbrokerのport、tokenはbrokerが検証）に変える。
   brokerが唯一の出口であることとtokenの認証は保つ。
   決定8の段(4)の「queueのbroker（TCPとtoken、egressのallowlist）」の「TCP」も同じくこの形に読み替える。
6. **ADR-t2113-3決定1を変える範囲。**
   containerに見せるのはworktreeとrun dirだけ、の例外にrunのunix socketを足す。
   socketは秘密もhostのファイルも持たず、brokerへの経路だけを与える。

## Alternatives

- **containerにbridgeのnetworkとegressのfilterを持たせる**: 宛先の制限をcontainerのfilterとbrokerの2か所で持ち、filterの抜け（DNS・IPの直接の指定・IPv6）を全部塞ぐ必要がある。
  `--network none`なら経路そのものが無い。
- **hostのTCPのportに直接届かせる（ADR-t1233-1決定6のまま）**: containerにnetworkが要り、gvproxyを通ってhostの他のportと外にも届く。
  brokerのportだけに絞るのはcontainerのfilterに頼ることになり、上の案と同じ問題が残る。
- **hostのunix socketをVMを越えてcontainerにmountする**: macOSのPodmanでは安定しない（ADR-t1233-1決定6の理由）。

## Consequences

- containerのrunの外への経路はbroker 1本で、brokerの宛先の制限が全部の外への通信に効く。
- relay・転送・socketの片付けと、relayのhealthの見張りが実装に増える。
  relayが落ちる間、containerのrunは外へ出られない。
- proxyを見ない道具（`curl`の素の呼び出し、git の`https`のremoteなど）はcontainerで動かない。
- machineのvolumeを絞ると、volumeの外のpath（人のhomeの下の別のrepository）をcontainerで使えないので、queueのrun dirとworktreeの置き場はvolumeの中に揃える。
