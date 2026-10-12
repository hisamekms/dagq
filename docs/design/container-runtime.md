---
id: design-container-runtime
type: design
title: Container runtime
status: current
created: 2026-10-11
scope: runtime
tags:
  - runtime
  - security
related:
  - adr-t2113-1
  - adr-t2113-2
  - adr-t2113-3
  - adr-t2114-1
  - adr-t2114-2
  - adr-t2114-3
  - adr-t2114-4
  - adr-t2173-1
  - adr-t2257-1
  - design-security
  - design-supervisor-lifecycle-run-environment
---

# Container runtime

信頼しないコードを走らせる実行側を、hostのプロセスの代わりにrunごとのcontainerで動かす仕組みの地図。
containerに移すのはworker（resumeを含む）とintegrateのverificationの2つの適用先だけで、読むだけのheadlessのjob・planner・inbox・制御側はhostに置く（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1〜3）。
containerを選ばない限りhostのrunの振る舞いは変わらない。

## 適用先と設定の読み込み

- 適用先ごとのruntimeは`dagq.toml`が持ち、読み込みの型は`domain::container_runtime::RuntimeConfig`で、設定の綴りと値はそのdoc commentが持つ。
- 読むのは`infrastructure::run_env::parse_config`（`load_runtimes`、`Verifier::runtimes`）。
  適用先でない役割にcontainerを書いた`dagq.toml`は、読み込みが理由つきで拒む（`refuse_container`）。
  inboxは`dagq.toml`の役割の表に無く、表を書けば知らない役割として拒む。
- supervisorは各passで読み直し（`supervise::container`）、`status`・`doctor`・`integrate`はそのたびにmain checkoutのファイルを読む。
  `status`・`doctor`とwrapperは、queueが束縛されたcheckoutから`BoundRuntimes`で読み、束縛の無いqueueはhost、束縛が解決できなければ読むたびにerrorになる。

## fail closedの境界

containerを選んだ適用先が使えないとき、hostで動かさずに控え、hostへ黙って戻さない（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4）。

- 使えるかは`ContainerRuntime`のport（`application::ports`）に問う。
  supervisorと`integrate`は`application::queue_broker::BrokerGate`越しに問い、queueのbrokerが動いていなければそれを理由に使えないと答える（[Queue broker](queue-broker.md#見張りと知らせ)）。
  brokerの奥では`application::podman_machine::MachineGate`がdagq専用のmachineを見て、使えなければ理由つきで使えないと答える（下の「dagq専用のmachine」）。
  その奥の、残りの部品が揃うまでの実装`infrastructure::container_runtime::UnbuiltContainer`は、常に理由つきで使えないと答える。
- worker: 各候補のclaimを控え、理由を`claim_deferred`に残す（[claimを控える](supervisor-lifecycle/claim-defer.md#workerを動かせないtask)）。
  resumeの候補からも外す（`Supervisor::container_deferral`）。
  resumeを使い切ったrunを人に渡すことは止めない。
  切り替えの前からhostで動いていたrunの次のturn（答えの配達・reviseなど）は、wrapperがturnを起動するところ（`headless_session`の`Turns`）で`dagq.toml`を読み直して拒み、wrapperはerrorで終わる。
  読めないときも、containerの設定かもしれないので同じく拒む。
  supervisorが起動するactor（`Supervisor::actors`）も同じくworkerの`backend`をruntimeに合わせる。
  containerが使えても、Codexに経路が向くrun（taskのproviderと`[provider_fallback]`で決まる行き先）は控える。
  queueのbrokerの宛先にOpenAIが無いため（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定2）。
- verification: supervisorは着地を始めずに待ち（`domain::landing_hold`）、drainではleaseを返して人に渡す。
  supervisorはtaskの検証のコマンドの有無を見ず、全ての着地を待たせる。
  `integrate`自身も検証のコマンドの前に確かめ（`integrate::container_refusal`）、どのコマンドもhostで流さずにrunを元のstatusへ戻す。
  containerが使えると答えても、verificationのcontainerの実行がまだ無いので同じく拒む。
- 知らせ: supervisorと、拒んだ`integrate`が、答えがqueueの最新の記録と違うとき（待つ適用先か理由が変わったときを含む、`ContainerCheck::transition`）に`container_runtime_unavailable` / `container_runtime_available`を記録し、前者はinbox宛てのattentionになる。
  supervisorを通さない`integrate`だけでもinboxに届く。
  人は部品が揃うのを待つか、runtimeをhostに戻すtaskを頼む。
- 直接の起動: `ExecutorBackend::Container`へactorを起動する経路（`HostActorExecutor`・reviewのprogramのjob）は`ensure_implemented`が拒む。
  containerが使えると答えても、workerのcontainerの実行が無い間はここで拒まれる。
- 適用先の外: 着地の前のe2eとmacOSに固有のtestはhostに置く（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定3）。
  verificationをcontainerにしても、runのe2eはhostで流れる。

## 見せ方

`status`と`doctor`は`runtimes`に適用先ごとのruntimeとenforcement（hostは`advisory`、containerは`sandbox`）を出し、`actors`のworkerの行の`backend`もworkerのruntimeに従う（`health::add_runtimes`、`execution::runtime_executions`）。

## dagq専用のmachine

containerはdagq専用の名前のPodman machineだけで動かす（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6、[ADR-t2173-1](../adr/2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)）。
判断は`domain::podman_machine`、use caseは`application::podman_machine`、podmanのコマンド・lock・`host.toml`の読み込みは`infrastructure::podman_machine`が持つ。

- 名前と接続: machineと接続の名前は`domain::podman_machine::MACHINE_NAME`で、podmanのコマンドはmachineの名前か`--connection`でそれを名指す。
  人の既定のmachineと既定の接続は変えず、作らず、止めない。
  Podmanの版によってはinitとstartが既定の接続を変えるので、`PodmanCli`が前の既定に戻す（前に既定が無ければ戻す先が無く、dagqの接続が既定に残りうる）。
- 起動と停止: `ensure`は無ければinitし、止まっていればstartし、使う者として数える入口で、containerの起動の前とpodmanを要るtestとスモークの前に呼ぶ（containerの実行の部品が揃うまでは呼ぶのは`#[ignore]`のtestだけ）。
  使い終えたら`release`を呼び、使う者が居なければ止める。
  どちらもhost全体のlock（data homeの`dagq/machine/`）の中で判定と操作を行うので、並行するrun・verification・testが同時に呼んでも作るのと起動するのは1回になる。
  使う者は記録したprocessが終わっていれば数えない。
- 人のmachine: Podmanは同時に1つのmachineしか動かさないので、人のmachineが動いている間はdagqのmachineを起動せず、containerを使えないと答える（fail closed）。
  人のmachineは止めない。
- 資源: CPU・メモリ・diskはhost全体の`host.toml`の`[machine]`が与え、既定値と根拠は`MachineResources`のdoc commentが持つ。
  machineはhostに1つで全てのprojectが共有するので、queueの`host.toml`では変えない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定6）。
  値はinitで使われ、変えた値は作り直したmachineに効く。
- hostのmount: initのvolumeは各queueのrun dir・cloneの親のdir・verificationのcloneの親のdirだけで（`MachineMounts`）、hostの`$HOME`・workerのcache・buildの出力・着地用のworktreeのdirを含めない。
  各dirはmachineの中の同じpathにmountする。
  macOSのremoteのPodmanのbind mountのsourceはmachineの側のpathなので、containerに見せるhostのdirはmachineにmountしてある必要がある。
  queueのrun dir・runのclone・verificationのcloneはこの3つのdirの下に置き、着地用のworktreeはどの下にも置かない。
  initはhostの全てのqueue（data homeの`dagq/`の下でqueueのDBを持つdir）のdirをmountするので、作った時点のqueueがmachineを共有する。
  machineにmountしても、containerに見せるかはcontainerごとのmountで決まる。
- machineの中のnamed volume: buildの出力はrunかverificationごと、workerのcacheはqueueごとのnamed volumeで、hostにmountしない（`MachineVolumes`、名前は`QueueKey`）。
  virtiofsのmountの上に置いたbuildの出力ではlinkが落ちたため（[ADR-t2173-1](../adr/2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)のContext）。
  名前がqueueとrunを含むので、あるqueueの操作は別のqueueのvolumeに届かない。
  runの終わりと着地でrunのbuildの出力を消すのはcloneと同じ片付けの経路で、使う者が去る前に`MachineVolumes`のportを呼ぶ（呼ぶのはその経路の実装）。
  machineを作り直すとvolumeは消え、workerのcacheは冷えた状態から始まる。
- 食い違いと作り直し: volumeはinitでしか決まらない。
  `ensure`と`MachineGate`は、queueのdirがmountされていないか、3つの種類の外のdir（`$HOME`など）がmountされていれば、作り直しの手順を添えて使えないと答える。
  作り直しは`podman machine stop dagq`と`podman machine rm dagq`で、次の`ensure`が今の`host.toml`とhostのqueueでinitする。
  作った後に足したqueueと資源の変更も、同じ手順で反映する。
- 使えるかの答え: `MachineGate`はlockを取らずmachineを変えずに見るだけで、無いか止まっているmachineは使えるとする（containerの起動の前の`ensure`が動かす）。
- 実物のpodmanで流すtestは`#[ignore]`で、関門に数えない。

## 後続の部品の置き場

次の部品が揃ったら、それぞれが`ContainerRuntime`の答えを埋める。
どれかが欠ければ使えないと答え、上の境界のまま控える。

- container image（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定6、[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定2）。
- hostのqueueのbrokerの`api.anthropic.com`と`dagq.toml`が挙げるpackage registryの宛てと長期tokenの差し替え（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)・[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)・[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)）。
  brokerのプロセスとqueue serviceの宛ては[Queue broker](queue-broker.md)。
- VMの中のrelayとrunごとのunix socket（[ADR-t2114-3](../adr/2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)）。
- runごとのcloneとbundleの取り込み（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）、containerの中のsccache（[ADR-t2114-4](../adr/2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)）。
- worker・resumeとverificationのcontainerの実行（`ExecutorBackend::Container`の実装）。
