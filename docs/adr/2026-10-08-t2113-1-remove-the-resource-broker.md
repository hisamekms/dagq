---
id: adr-t2113-1
type: adr
title: resource broker（dagq-broker）を外し、実行側は隔離環境の中で組み込みの道具をそのまま使う（ADR-t827-1〜4・t838-1・t840-1を置き換える）。ADR-t827-3のうちworkerのcontainerでも要るmachineの決定（専用のmachine・冪等なinit・start・stopとlock・最小の資源とhardening）は書き直して引き継ぐ
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
supersedes:
  - adr-t827-1
  - adr-t827-2
  - adr-t827-3
  - adr-t827-4
  - adr-t838-1
  - adr-t840-1
owners:
  - hisamekms
tags:
  - runtime
  - security
  - broker
related:
  - adr-t2113-2
  - adr-t2113-3
  - adr-t1233-1
  - adr-t728-1
  - adr-t728-2
  - design-broker
  - design-security
---

# ADR-t2113-1: resource brokerを外し、実行側は隔離環境の中で組み込みの道具をそのまま使う（ADR-t827-1〜4・t838-1・t840-1を置き換える）

## Context

resource broker（`dagq-broker`）は、workerのfs・process・git・packageの操作をqueueごとに1つの共有のcontainerで仲介し、runごとのtokenで閉じ込め、opごとにauditを残す（[ADR-t827-1](2026-09-28-t827-1-broker-crates-binaries-and-version-alignment.md)〜[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)・[ADR-t838-1](2026-10-05-t838-1-required-broker-mode-refuses-built-in-tools-and-holds-claims.md)・[ADR-t840-1](2026-10-05-t840-1-broker-package-backend-runs-only-configured-commands.md)）。
workerはhostのまま（助言的、ADR-t728-1決定6）で、brokerは隔離ではなく、workerのcontainer化（Phase 3）に進むための契約の証明と位置づけていた。
この repositoryの本番queueではdisabledのまま使っていない。

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)は制御側と実行側を分け、実行側を隔離環境に置き、実行側から制御側への唯一の出口をqueueのbrokerにした。
runごとのcontainerで実行側を動かせば、境界はcontainerそのもので、workerはcontainerの中のファイルとプロセスを組み込みの道具で直接扱える。
そのときresource brokerが独自に足すのは、gitの扱い（pushを持たない・共通dirのconfigとhooksを守る）とopごとのauditだけになる。

一方、queueごとに1つの共有のcontainerは、同じqueueの全てのrunのworktree・gitの共通dir・tokenの署名の鍵・有効な印・auditを見る（ADR-t827-2決定6が既知の制限として認めたもの）。
1つのrunの`process.exec`から他のrunのファイルとtokenの偽造に届くので、runごとのcontainerに比べてかえって攻撃面が広い。
人とinboxは2026-10-08に、resource brokerを外し、brokerは隔離の外へ出る経路（queueのbroker）にだけ用意すると合意した。

## Decision

1. **resource brokerを外す。**
   fs・process・git・packageを仲介するcontainer・server・client・MCPの道具・runごとのtoken・opごとのaudit・`disabled` / `preferred` / `required`のmodeは、どれも残さない。
   実行側（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)が決める範囲）は、隔離環境の中で組み込みの道具（Read・Edit・Write・Bashなど）をそのまま使う。
   境界は隔離環境（runごとのcontainerかOSのsandbox）が持ち、道具の層では作らない。
2. **gitはmountの設計で守る。**
   resource brokerが持っていたgitの不変条件（pushを持たない・上流の資格情報を置かない・hostのgitの設定とhookを守る）は、実行側に何をmountするかで成り立たせる（[ADR-t2113-2](2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）。
3. **brokerは隔離の外へ出る経路だけに置く。**
   実行側から制御側と外部へ出る経路は、ADR-t1233-1決定6のqueueのbrokerだけにする。
   その形（起動と停止・宛先とallowlist・認証・workerのcontainerからの通信経路）は後続のADRが決める。
4. **dagq専用のmachine（ADR-t827-3決定4の引き継ぎ）。**
   podmanを使う実行側のcontainerは、dagq専用の名前のPodman machineだけで動かし、podmanのコマンドはそのmachineの接続を明示して打つ。
   人の既定のmachineと既定の接続は変えず、作らず、止めない。
   人のmachineが動いていて専用のmachineを起動できないときは、containerの実行側を使えないものとして扱って人に知らせ、hostの実行に黙って戻さない（fail closed）。
5. **init・start・stopは冪等でlockする（ADR-t827-3決定5の引き継ぎ）。**
   machineが無ければ最小の資源でinitし、止まっていればstartする。
   これをcontainerを起動する前とpodmanを要るtestとスモークの前に行い、host全体のlockで直列にして、並行するrun・verification・testが同時に呼んでも壊れないようにする。
   machineを使うものが居なくなったら止め、止める判定と停止も同じlockの中で行う。
   podmanを要るtestとスモークも、終わりに同じ判定で止める。
6. **最小の資源とcontainerのhardening（ADR-t827-3決定7の引き継ぎ）。**
   machineの資源は、そこで動かすもの（worker・resumeとintegrateのverificationのbuildとtest）が通る最小を測って決め、決めた値はdesignに書き、`host.toml`で上書きできる。
   containerはmemory・cpu・pidsの上限を持ち、非rootで動き、root filesystemを読み取り専用にし（書けるのはmountしたものと一時の領域だけ）、capabilityを落とし、特権の昇格を禁じる。
   containerにPodman / Dockerのsocketを渡さない。

ADR-t827-3のうちmachineのvolume（決定6、既定でhostの`$HOME`をVMにmountする）の見直しは、containerに見せる範囲を決める[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)と、VMの中に通信の中継を置く後続のqueueのbrokerのADRに回す。
ADR-t827-3のその他の決定（queueごとの常駐のcontainer・supervisorのhealthと通知・imageのbuild・`process.exec`の制限）はresource brokerの前提なので引き継がない。

## Alternatives

- **resource brokerをrunごとのcontainerに作り直す**: runごとのcontainerなら境界はcontainerで、brokerはcontainerの中の操作を仲介する層を1つ足すだけになる。
  残る価値はopごとのauditとgitの扱いで、gitはmountで守れ、auditのためにworkerの道具をMCPに替えて組み込みの道具を拒む費用（道具の不足・promptの説明・providerごとの対応、今のdagqはCodexのworkerにMCPの道具を渡せない）に見合わない。
- **今の形のまま残す**: 共有のcontainerが全てのrunのworktreeと鍵を見る攻撃面が残り、workerのcontainer化の段でrunごとの閉じ込めを作り直す必要がある（ADR-t827-2決定6）。
- **opごとのauditのためだけに残す**: 誰が何を変えたかはrun branchのcommitとdiff、queueのevent、queueのbrokerを通る外への要求の記録で読める。
  containerの中の操作の一つ一つを残すことは、境界にも判断にも使っていない。

## Consequences

- resource brokerのコード（`crates/`のbrokerのcrate、dagqの用意・token・supervisorの統合・管理のコマンド、`[broker]`の設定、`up`のpreflightと`down`の停止、podmanの`#[ignore]`のtestとe2eの`broker::`）と、その地図・pluginのreferenceの撤去の実装は、このADRの着地の後に別のgoalで行う。
  それまではコードにresource brokerが残り、design（[Resource broker](../design/broker.md)）は撤去予定の今の姿を書く。
  この間にresource brokerへ機能を足さない。
- resource brokerの前提で置き換えた決定に依る他のADRは、撤去の実装とともに意味を失う。
  [ADR-t828-1](2026-09-28-t828-1-coverage-gate-covers-the-workspace-with-workspace-flag.md)（ADR-t827-1決定3のamends。workspaceの`default-members`と関門の`--workspace`）は、crateが残る間はそのまま有効で、brokerのcrateが無くなった後のworkspaceの形は撤去の実装が決める。
  brokerのe2eに依るADR（ADR-t1162-1・ADR-t1582-1など）も撤去の実装が扱う。
- opごとのauditが無くなり、containerの中の操作は記録されない。
  記録は、run branchのdiff・queueのevent・queueのbrokerを通る外への要求に限られる。
- crates.ioへのpublishとinstallの差し替えの対象はdagqの1本に戻る（撤去の実装の後）。
- 引き継いだmachineの決定は、workerのcontainer化とintegrateのverificationの隔離の実装（goal 38の段(4)・(6)）が使う。
  資源の値はresource brokerの値（CPU 1・メモリ1 GiB）ではworkerのbuildが通らない見込みで、その実装が測って決め直す。
