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
  - adr-t2179-1
  - adr-t2257-1
  - adr-t2182-1
  - adr-t2424-1
  - adr-t3163-1
  - design-security
  - design-supervisor-lifecycle-run-environment
---

# Container runtime

信頼しないコードを走らせる実行側を、hostのプロセスの代わりにrunごとのcontainerで動かす仕組みの地図。
containerに移すのはworker（resumeを含む）とintegrateのverificationの2つの適用先だけで、読むだけのheadlessのjob・planner・inbox・制御側はhostに置く（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1〜3）。
containerを選ばない限りhostのrunの振る舞いは変わらない。
実物のpodmanとmachineで流すtestは`#[ignore]`で、関門に数えない。

## 適用先と設定の読み込み

適用先ごとのruntimeは`dagq.toml`が持ち、読み込みの型は`domain::container_runtime::RuntimeConfig`で、設定の綴りと値はそのdoc commentが持つ。
読み込みと役割の拒否は`infrastructure::run_env::parse_config`、queueの束縛からの読み込みは`BoundRuntimes`を入口にする。
束縛が解決できなければ読むたびにerrorになり、wrapperはturnをhostで起動しない。

## fail closedの境界

containerを選んだ適用先が使えないとき、hostで動かさずに控え、hostへ黙って戻さない（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4）。

- 使えるかは`application::ports::ContainerRuntime`に問い、部品ごとのgateが理由つきで答える。
  部品ごとの条件は[Queue broker](queue-broker.md#見張りと知らせ)・[Claudeの認証](queue-broker.md#claudeの認証の差し替え)（[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)決定3）と下の各節が持つ。
- worker: 各候補のclaimを控え、理由を`claim_deferred`に残す（[claimを控える](supervisor-lifecycle/claim-hold.md#workerを動かせないtask)）。
  resumeの候補からも外す（`Supervisor::container_deferral`）。
  resumeを使い切ったrunを人に渡すことは止めない。
  切り替えの前からhostで動いていたrunの次のturn（答えの配達・reviseなど）は、wrapperがturnを起動するところ（`headless_session`の`Turns`）で`dagq.toml`を読み直して拒み、wrapperはerrorで終わる。
  読めないときも、containerの設定かもしれないので同じく拒む。
  逆にcloneで働くrunのturnも、workerがhostに戻った後はhostで起動せずに拒む。
  actorのbackendとhostに残るwrapperの境界は`Supervisor::actors`が持つ。
  containerが使えても、Codexに経路が向くrun（taskのproviderと`[provider_fallback]`で決まる行き先）は控える。
  queueのbrokerの宛先にOpenAIが無いため（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定2）。
- verification: supervisorは着地を始めずに待ち（`domain::landing_hold`）、止めるdrainではleaseを返す（handoffは返さない）。
  supervisorはtaskの検証のコマンドの有無を見ず、全ての着地を待たせる。
  `integrate`自身も検証のコマンドの前に確かめ（`integrate::verification_runtime`）、どのコマンドもhostで流さずにrunを元のstatusへ戻す。
  `already_satisfied`のsnapshotの検証も使えない間は始めずに待ち（[Validation](supervisor-lifecycle/validation.md#already_satisfiedの検証)）、hostで流さない。
- 知らせ: supervisorと、拒んだ`integrate`が、答えがqueueの最新の記録と違うとき（待つ適用先か理由が変わったときを含む）に`container_runtime_unavailable` / `container_runtime_available`を記録し、前者はinbox宛てのattentionになる。
  supervisorを通さない`integrate`だけでもinboxに届く。
  人は部品が揃うのを待つか、runtimeをhostに戻すtaskを頼む。
- 直接の起動: hostのexecutor（`HostActorExecutor`・reviewのprogramのjob）は`ExecutorBackend::Container`に置いたactorを起動せず、`ensure_implemented`が拒み、containerはwrapperと`integrate`だけが下の2節で起動する。
- 適用先の外: 着地の前のe2eはhostに置き（`[e2e]`の宣言、[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定4）、verificationをcontainerにしてもrunのe2eはhostで流れる。
  containerはLinuxなので、OSに固有のtest（このrepositoryならmacOSのもの）はcontainerのverificationでbuildされず流れず、runtimeはhostに回さない。
  このrepositoryはそれをmainのCIのmacOSのjobに任せる（[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)）。

## 見せ方

`status`・`doctor`の`runtimes`とactorの見せ方は`health::add_runtimes`を入口にする。

## dagq専用のmachine

containerはdagq専用の名前のPodman machineだけで動かす（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6、[ADR-t2173-1](../adr/2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)）。
判断は`domain::podman_machine`、use caseは`application::podman_machine`、podmanのコマンド・lock・`host.toml`の読み込みは`infrastructure::podman_machine`が持つ。

- 名前と接続: machineと接続の名前は`domain::podman_machine::MACHINE_NAME`で、podmanのコマンドはmachineの名前か`--connection`でそれを名指す。
  人の既定のmachineと既定の接続は変えず、作らず、止めない。
- 起動と停止は`application::podman_machine::ensure`を入口にする。
  host全体のlockで判定と操作を行い、並行しても作るのと起動は1回になる。
  人の既定の接続を復元できるのは前の既定がある場合だけで、無ければdagqの接続が既定に残りうる。
- 人のmachine: Podmanは同時に1つのmachineしか動かさないので、人のmachineが動いている間はdagqのmachineを起動せず、containerを使えないと答える（fail closed）。
  人のmachineは止めない。
- 資源: CPU・メモリ・diskはhost全体の`host.toml`の`[machine]`が与え、既定値と根拠は`MachineResources`のdoc commentが持つ。
  machineはhostに1つで全てのprojectが共有するので、queueの`host.toml`では変えない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定6）。
- メモリの予約の判断と値の根拠は`domain::machine_memory`、待ちの入口は`application::podman_machine::wait_for_memory`（[ADR-t3163-1](../adr/2026-10-25-t3163-1-containers-reserve-memory-on-the-machine.md)）。
  待ちはrunのevent（[計測](measurement.md#区間とタグ)）と`status`に出る。
  wrapperは待つ間も終了の依頼と柵を読み、当たればturnを始めずに終わる。
- hostのmount: initのvolumeは各queueのrun dir・cloneの親のdir・verificationのcloneの親のdirだけで、hostの`$HOME`・workerのcache・buildの出力・着地用のworktreeのdirを含めない。
  各dirはmachineの中の同じpathにmountする。
  macOSのremoteのPodmanのbind mountのsourceはmachineの側のpathなので、containerに見せるhostのdirはmachineにmountしてある必要がある。
  queueのrun dir・runのclone・verificationのcloneはこの3つのdirの下に置き、着地用のworktreeはどの下にも置かない。
  initはhostの全てのqueue（data homeの`dagq/`の下でqueueのDBを持つdir）のdirをmountするので、作った時点のqueueがmachineを共有する。
  machineにmountしても、containerに見せるかはcontainerごとのmountで決まる。
- machineの中のnamed volume: buildの出力はrunかverificationごと、workerのcacheはqueueごとのnamed volumeで、hostにmountしない（`MachineVolumes`、名前は`QueueKey`）。
  名前がqueueとrunを含むので、あるqueueの操作は別のqueueのvolumeに届かない。
  runの終わりのvolumeの片付けは`application::broker_relay::RelayControl::close`を入口にする。
  machineを作り直すとvolumeは消え、workerのcacheは冷えた状態から始まる。
- 食い違いと作り直し: volumeはinitでしか決まらない。
  `ensure`と`MachineGate`は、queueのdirがmountされていないか、`dagq/`の下のqueueの3つのdirでないdir（`$HOME`、別の場所の同名のdir）がmountされていれば、作り直しの手順を添えて使えないと答える。
  作り直しの入口は`domain::podman_machine::mounts_mismatch`、使えるかの読み取りは`application::podman_machine::look`。
  作った後に足したqueueと資源の変更も、machineの作り直しを要する。

## image

containerのimageは、runtimeが持つbaseと、`dagq.toml`が名指すprojectのContainerfileの2段である（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定2）。
判断は`domain::container_image`、use caseは`application::container_image`、podmanのコマンドとmainのcheckoutの読み取りは`infrastructure::container_image`が持ち、queueの記録はsupervisorとwrapperではqueue serviceを通る（fail closed）。

clientとbaseの構成・入手は`domain::container_image::ClientSource`を入口にする。
baseに言語の道具を残さない。
dagqのrepositoryにpushされていないcommitは取れず、baseのbuildが落ちる。
hostにdagqのcheckoutは要らない。
projectのimageの設定は`domain::container_image::ProjectImage`を入口にする。
入力は制御側のmainのcheckoutから読み、runのcloneからは読まないので、着地前のworkerの変更でimageは変わらない。
このrepositoryのContainerfileは`.dagq/container/Containerfile`である。
imageのtagの入口は`domain::container_image::Images::of`、buildと片付けと失敗時の控えは`application::container_image::prepare`。
projectのimageはqueueごとに分かれ、片付けは別のqueueに届かない。
baseはhostの全てのqueueが共有し、使用中のimageはpodmanが残す。
秘密情報とhostのpathはimageに入らない。

## brokerへの経路

runのcontainerは`--network none`で、外への経路はhostのqueueのbrokerへの1本だけにする（[ADR-t2114-3](../adr/2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)決定1〜4）。
relayと転送の責務の境界は`domain::broker_relay`で読む。

socketとrelayの起動・終了は`application::broker_relay::ready_for_turn`を入口にする。
relayとそのrunのcontainerだけがsocketのvolumeをmountする。
relayと転送はtokenを持たず、認証も宛ての判定もしない（brokerが行う）。
転送とenvの入口は`domain::broker_relay::container_env`。
道具に固有の設定を持たない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）。
クライアントモードの宛ては[Queue service](queue-service.md#クライアントモード)。
- DNSが無い: proxyのenvを見ない道具は名前を引けず外へ出られない。
  これは隔離の結果として扱い、要るものはhostのrunかqueue serviceのユースケースに回す。
relayの見張りと起動し直しは`application::supervise::host_ops::broker_relay`を入口にする。
attentionが立つ間は`RelayGate`がcontainerを使えないと答え、新しいrunを控える（上の「fail closedの境界」）。

## git

containerで動くrunは、hostのgitの共通dirを共有しない独立したclone（`<queue dir>/clones/<run>`）で働く（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定1・3、[ADR-t2179-1](../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)）。
runのworktreeのpathにcloneを記録したrunがcontainerのrunで、hostのrunはworktreeのまま変わらない。
制御側の読み方と扱い方の分かれ目は`application::run_git`の1か所にあり、呼び出し元はcloneのpathをgitに渡さない。

- cloneの作成と事実の読み取りは`infrastructure::adapters::run_clones`を入口にする。
  remote・credential helper・上流の資格情報は置かない。
  読めない・食い違う事実はsessionに返す（pollは書きかけとして待つ）。
- 事実だけのturn: 最後のturnの終わりのものでない事実は、headを読む前にagentもbrokerへの経路も無いturnで書き直す（[ADR-t2978-1](../adr/2026-10-23-t2978-1-facts-only-turn-runs-in-a-narrow-container-with-guarded-git.md)）。
  supervisorのpassを塞ぎ続けない時間の上限とcold buildの打ち切りは`infrastructure::run_container::FACTS_TURN_TIMEOUT`が持つ。
- 再計画の保留と復元: 書き出した作業と渡す材料による（[再計画](../plans/task-replanning.md)）。
- 着地・review・e2e: 制御側だけの`<queue dir>/landing/<run>`の着地用のworktree（着地する`result_commit`をdetachedで置く）で動く。
  どのmachineとcontainerのmountにも入らず、作れないときは着地せず人を待つ。
- mainの受け渡しは`GitRepository::hand_main`を入口にする。
- hookとdriver: containerのrunのcommitを扱う制御側のgitは、hookを無いpathに向け、制御側のconfigの全scopeが定義するfilter・diff・mergeのdriverをプログラムを走らせないものに上書きして走る。
  着地用のworktreeで起こすプロセスにも同じ上書きをenvで渡し、上書きが効くと確かめられなければ着地しない。
- run dirのファイル: symlinkを辿らずに読み書きする。
- 片付け: worktreeと同じ契機で、clone・着地用のworktree・制御側のrun branch・run dirの事実と復元のファイルを、gitを使わずsymlinkを辿らずに消す。

## workerの実行

cloneで働くrunの各turn（最初のturn・revise・答え・resume）は、hostのprocessの代わりにdagq専用のmachineのcontainerで動く（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1、[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)決定1）。
session wrapperはhostの制御側に残り、turnの出力・receipt・askをrun dirから読み、`run log`で追える。
workerの実行全体は`application::run_container::PodmanActorExecutor`を入口にし、container内の手順は`infrastructure::container_turn`で読む。

- 準備は`infrastructure::run_container::PodmanRunContainers`を入口にする。
  どれかが失敗すればturnを起動せず、hostにも戻さない。
- token: supervisorが発行したworkerのtokenを、run dirのファイル（mode 0600、renameで置くのでworkerが置いたsymlinkを辿らない）にして、その場所をenvで渡す。
  podmanの引数とcontainerの設定にtokenの値は無く、ファイルはcontainerを消すときに消す。
- hardeningと上限は`domain::run_container::worker_run_args`を入口にする。
  Podman / Dockerのsocketを渡さない。
  mountはrunのclone・run dir・runのsocket・`dagq.toml`が名指すvolumeだけで、着地用のworktreeとverificationのcloneはmountしない（reviewの間も残るworkerのcontainerに、制御側が着地と検証に使う物を見せない）。
  podmanのcwdはrun dirで、制御側はcloneでプログラムを起動しない（上の「git」）。
- buildの出力とworkerのcache: `dagq.toml`の`[container.build_output]`が名指すdirにrunごとのvolumeを、`[container.worker_cache]`が挙げるdirごとにqueueのvolumeをmountする（綴りと意味は`run_env`の各表のdoc comment、案は[profile](toolchain-profiles.md#推奨の設定)）。
  runtimeは道具を知らず、projectが`[run.env]`で向ける。
  workerのcacheは信頼せず、同じqueueのworker・resumeだけが共有する（ADR-t2257-1決定5）。
- sccache: containerの中のserverを使う（[sccacheのserver](supervisor-lifecycle/sccache.md#sccacheのserver)）。
- containerの中のagentの前後の手順は`infrastructure::container_turn`を入口にする。
  agentの結果に依らず事実を残し、次のturnがsessionをresumeできる。
- transcriptの探索は`infrastructure::transcripts`を入口にする。
- 止める: turnの終わりごとにwrapperがcontainerを消す（podmanのprocessを止めてもcontainerは残るため）。
  sessionを止めるどの経路もcontainerを消し、消せないときはlogに残してrunの終わりの片付けに任せる。
  消去の入口は`RecordingSessions`。

## verificationの実行

終了後OOM→`run_container::read_oom`。
診断のみ;分類/retry/held/handoff/flaky不変;成功/控え不可;Podman died.OOMKilled≠kill;見逃し得る;読めない≠OOMなし。

`dagq.toml`がverificationをcontainerにしたrunの着地では、rebaseの後の検証のコマンドを1本ずつ、そのverificationのcontainerで流す（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1）。
結果・時間・失敗の分類の記録はhostと同じである。
`integrate`の`ContainerVerification`が流れを持ち、準備・podman・片付けは`application::run_container::VerificationContainers`のport、containerの中は`dagq relay verify`が持つ。
着地の再確認のcommandも同じ形で流す（[landing recheck](supervisor-lifecycle/integrate.md#landing-recheck)）。
`already_satisfied`のsnapshotの検証も同じ部品で、mainのheadのcloneで流す。
podmanがcontainerを起動しなかった（`podman_failed`）commandは判定せずhostで流さず、`integrate`は準備が落ちたときと同じくerrorで終える。

準備とverificationのdirの条件は`integrate::ContainerVerification`を入口にする。
containerが始まった後は、制御側はこのcloneでgitを走らせない。
準備のコマンドの契約は`run_env::CONTAINER_VERIFICATION_TABLE`が持つ。
runのコードはhostで走らせず、準備が落ちれば検証のコマンドを流さない。

- container: workerのturnと同じhardeningと`--network none`で、mountはverificationのdir・run dirの材料のdir（読み取り専用）・runのsocket・そのverificationのbuildの出力のvolumeだけにする。
  着地用のworktree・workerのclone・run dirの他のファイル・workerのcache・hostのsupervisorのcacheはmountしない（[ADR-t2114-4](../adr/2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)決定4）。
  コマンドごとの上限とcontainerの消去は`ContainerVerification::run_to_log`を入口にする。
- token: verificationのtokenを写しのファイルで渡し（podmanの引数に値は無い）、`dagq relay verify`が標準のproxyのenvに入れる。
  queueのbrokerはこのtokenを`dagq.toml`のregistryのallowlistへのtunnelにだけ通し、verificationの終わりに取り消す（[Queue broker](queue-broker.md)）。
- cacheの扱いは`integrate::without_wrapper`を入口にする。
  verificationで保護されていないcacheは使わない。
- 片付け: verificationがどう終わっても、container・buildの出力のvolume・dirを消す。
  dirはgitを使わずfdから辿り、残ったcontainerがsymlinkに差し替えても外に届かない。
- hostの絶対pathを名指すverifyは、containerにそのpathが無いので落ち、hostに回さない。
  verifyはimageの`PATH`の道具を名前で名指し、要る道具はprojectのimageに入れる（[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)決定3）。

## 後続の部品の置き場

次の部品が揃ったら、それぞれが`ContainerRuntime`の答えを埋める。
どれかが欠ければ使えないと答え、上の境界のまま控える。

- verificationの保護されたcache。
