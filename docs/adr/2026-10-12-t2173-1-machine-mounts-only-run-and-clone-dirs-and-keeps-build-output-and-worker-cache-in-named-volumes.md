---
id: adr-t2173-1
type: adr
title: dagq専用のmachineはhostからrun dir・cloneの親のdir・verificationのcloneの親のdirだけをmountし、buildの出力（runとverificationごと）とworkerのcache（queueごと）をmachineの中のnamed volumeに置く（ADR-t2114-3決定4のmachineのvolumeの項・ADR-t2113-3決定1をamends）
status: accepted
created: 2026-10-12
updated: 2026-10-12
accepted_on: 2026-10-12
amends:
  - adr-t2114-3 decision 4
  - adr-t2113-3 decision 1
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t2113-1
  - adr-t2113-2
  - adr-t2113-3
  - adr-t2114-3
  - adr-t2114-4
  - adr-t2257-1
  - design-container-runtime
  - design-security
---

# ADR-t2173-1: machineはrun dir・cloneの親のdir・verificationのcloneの親のdirだけをmountし、buildの出力とworkerのcacheはmachineの中のnamed volumeに置く

## Context

[ADR-t2114-3](2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)決定4のmachineのvolumeの項は、dagq専用のmachineにhostの`$HOME`をmountせず、containerに見せるもの（clone・run dir・[ADR-t2114-4](2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)のworkerのcache）の親のdirだけをvolume（macOSではvirtiofs）にしてinitすると決めた。
buildの出力（cargoなら`CARGO_TARGET_DIR`）はcloneかcacheの下、つまりmountの上に置かれる。

goal 38のSpikeの(c)の1回目（goal 38のnote 152044）では、buildの出力をmachineにvirtiofsでmountしたhostのdirに置くと、どの資源の組でもbuild scriptのlinkが「ld: cannot find .../build_script_build-*.rcgu.o」で落ちた。
machineの中のnamed volumeに置くと、4 CPU / 8 GiBでbuildが343秒で通った（sourceとtoolchainはmountの上のままで通った）。
(d)（note 152045）も同じ置き場で26のbuildのうち25が落ちた。
workerのcacheもbuildの道具が多数の小さいファイルを書き読みするので、同じ置き場の問題を受けうる。

integrateのverificationのcloneは、制御側がhostのgitでverificationごとに作り、verificationのcontainerに見せる。
macOSのremoteのPodmanのbind mountのsourceはmachineの側のpathなので、machineにmountしていないhostのdirはcontainerにmountできない。

[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1は、containerに見せるのをworktree（clone）とrun dirだけにした。
例外は、ADR-t2114-3決定6のrunのunix socketと、ADR-t2114-4のworkerのcacheのdirとverificationの保護されたcacheである。

## Decision

1. **hostからmachineへのmountは3種のdirだけにする。**
   hostのsupervisorが読み書きするrun dir、制御側が作るrunのcloneの親のdir、制御側がintegrateのverificationごとに作るverificationのcloneの親のdirだけをvolumeにする。
   verificationのcloneの親のdirは制御側だけのdirで、run dir・cloneの親のdir・着地用のworktreeのdirのどれとも別で、互いに含まない。
   これをmountするのは、制御側がhostのgitで作るverificationのcloneをverificationのcontainerに見せるためである。
   machineにmountしても、containerに見せるかはcontainerごとのmountで決まる。
   worker・resumeのcontainerはverificationのcloneの親のdirをmountせず、verificationのcontainerはそのverificationのdirだけをmountする。
   着地用のworktreeのdirはmachineにmountしない。
   hostの`$HOME`をmountしないことは変えない。
2. **buildの出力はmachineの中のnamed volumeに置く。**
   runtimeはrunごとのbuildの出力のnamed volumeをmachineの中のpodmanに作り（hostにmountしない）、runの終わりと着地で消す。
   containerの中のどのdirにmountするか（例えば`CARGO_TARGET_DIR`を向けるdir）はprojectの設定が与え、runtimeは言語の道具を知らない（[ADR-t2257-1](2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定1）。
3. **workerのcacheもmachineの中のnamed volumeに置く。**
   置き場はhostのmountではなく、machineの中のqueue（project）ごとのnamed volumeにし、別のprojectと共有しない（ADR-t2257-1決定5）。
   hostはworkerのcacheを読まず（信頼しないcache、ADR-t2114-4決定4）、machineを作り直せば消えてよい。
4. **containerに見せる範囲の例外にbuildの出力のvolumeを足す。**
   ADR-t2113-3決定1の「containerに見せるのはworktree（clone）とrun dirだけ」の例外に、buildの出力のnamed volumeを足す。
   worker・resumeのcontainerには、そのrunのbuildの出力のvolumeだけを見せる（runごと。そのrunのworker・resumeだけがmountし、別のrun・verificationと共有しない）。
   integrateのverificationのcontainerには、そのverificationごとの新しいbuildの出力のvolumeだけを見せる（workerのrunのvolumeと共有せず、verificationの終わりに消す）。
   どちらも、projectの設定がそのdirを名指すときだけmountし、hostのdirを見せない。
   ADR-t2114-4が足したworkerのcacheの例外は、見せる相手（worker・resumeのcontainerだけで、verificationにはmountしない）を保ったまま、置き場をhostのdirから決定3のqueueごとのnamed volumeに変える。
   runのsocket（ADR-t2114-3決定6）とverificationの保護されたcache（ADR-t2114-4）の例外は変えない。
   buildの出力とworkerのcacheはmachineの中のvolumeでhostのファイルを持たず、見せる相手をrun・verificationごとに分けるので、決定1が守る「hostのファイルと他のrunを見せない」境界を保つ。

## 既存の決定との関係

- ADR-t2114-3決定4: machineのvolumeの項の「containerに見せるもの（clone・run dir・workerのcache）の親のdirをvolumeにする」を、上の決定1の3種のdirに変え、workerのcacheとbuildの出力をvolumeの外（machineの中のnamed volume）に移す。
  `$HOME`をmountしないこと、volumeはinitでしか決まらず範囲を変えるときはmachineを作り直すことは保つ。
- ADR-t2113-3決定1: containerに見せる範囲の例外に、上の決定4のbuildの出力のvolumeを足し、workerのcacheの例外の置き場を変える。

元のADRの本文は書き換えない。

## Alternatives

- **今の形（clone・run dir・cacheの親のdirのmountの上にbuildの出力も置く）**: Spikeの(c)と(d)で、buildの出力をvirtiofsのmountの上に置くとbuild scriptのlinkが落ちた。
- **cloneもmachineの中のvolumeに置く**: cloneはhostの制御側がhostのgitで作り、integrateがrun dirのbundleを読むので、hostから見える必要がある。
  sourceの読み取りはmountの上で通った。
- **buildの出力をmountの上に置き、virtiofsの設定で直す**: 原因が特定できておらず、Podmanの版に依る。
- **verificationのcloneをmountせず、machineの中のnamed volumeに写す**（`podman volume import`などでtarを流す）: 写しの手間がverificationごとに増え、tarの作成でworkerのcommitのsymlinkや特殊なファイルを扱う検査が要る。
  sourceの読み取りはmountの上で通っているので、mountを1つ足す方が単純である。

## Consequences

- runtimeはmachineの中のnamed volumeを作る・消す・在るかを確かめる操作を持ち、名前でqueueとrunを分け、別のqueueのvolumeを消さない。
  runの終わりと着地でrunのbuildの出力のvolumeを消すのは、cloneの片付けと同じ契機の片付けの経路が行う。
- machineのvolumeは作ったときのものに固定されるので、queueのrun dir・cloneの置き場・verificationのcloneの置き場はmountの中に揃え、着地用のworktreeの置き場はどのmountの下にも置かない。
  設定とmachineのvolumeが食い違えば、containerを使えないものとして知らせ、人がmachineを作り直す。
- buildの出力とworkerのcacheはmachineのdiskを使い、machineを作り直すと消える。
  hostの`target/`やcacheは使い回されないので、containerのrunの最初のbuildは冷えた状態から始まる。
- 測り直し（Spikeの(c)・(d)）はこの形でbuildの出力とsccacheのcacheをnamed volumeに置いて確かめる。
