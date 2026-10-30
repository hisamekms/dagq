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

- 適用先ごとのruntimeは`dagq.toml`が持ち、読み込みの型は`domain::container_runtime::RuntimeConfig`で、設定の綴りと値はそのdoc commentが持つ。
- 読むのは`infrastructure::run_env::parse_config`。
  適用先でない役割にcontainerを書いた`dagq.toml`は、読み込みが理由つきで拒む（`refuse_container`）。
  inboxは`dagq.toml`の役割の表に無く、表を書けば知らない役割として拒む。
- supervisorは各passで読み直し（`supervise::execution::container`）、`status`・`doctor`・`integrate`はそのたびにmain checkoutのファイルを読む。
  `status`・`doctor`とwrapperは、queueが束縛されたcheckoutから`BoundRuntimes`で読み、束縛の無いqueueはhost、束縛が解決できなければ読むたびにerrorになる。

## fail closedの境界

containerを選んだ適用先が使えないとき、hostで動かさずに控え、hostへ黙って戻さない（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4）。

- 使えるかは`ContainerRuntime`のport（`application::ports`）に問う。
  supervisorと`integrate`は外から`BrokerGate`・`RelayGate`・`ClaudeCredentialGate`・`MachineGate`・`ImageGate`・`ContainerExecutors`の順に問い、各gateは自分の部品（queueのbroker・runのrelayの見張りの記録・workerの適用先の`host.toml`のClaudeの資格情報・dagq専用のmachineとそのメモリ・image）が使えなければ理由つきで使えないと答え、最も奥は両方の適用先に使えると答える。
  部品ごとの条件は[Queue broker](queue-broker.md#見張りと知らせ)・[Claudeの認証](queue-broker.md#claudeの認証の差し替え)（[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)決定3）と下の各節が持つ。
- worker: 各候補のclaimを控え、理由を`claim_deferred`に残す（[claimを控える](supervisor-lifecycle/claim-hold.md#workerを動かせないtask)）。
  resumeの候補からも外す（`Supervisor::container_deferral`）。
  resumeを使い切ったrunを人に渡すことは止めない。
  切り替えの前からhostで動いていたrunの次のturn（答えの配達・reviseなど）は、wrapperがturnを起動するところ（`headless_session`の`Turns`）で`dagq.toml`を読み直して拒み、wrapperはerrorで終わる。
  読めないときも、containerの設定かもしれないので同じく拒む。
  逆にcloneで働くrunのturnも、workerがhostに戻った後はhostで起動せずに拒む。
  supervisorが起動するactor（`Supervisor::actors`）も同じくworkerの`backend`をruntimeに合わせ、runのsession wrapperだけは制御側としてhostで起動する。
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

`status`と`doctor`は`runtimes`に適用先ごとのruntimeとenforcement（hostは`advisory`、containerは`sandbox`）を出し、`actors`のworkerの行の`backend`もworkerのruntimeに従う（`health::add_runtimes`）。

## dagq専用のmachine

containerはdagq専用の名前のPodman machineだけで動かす（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6、[ADR-t2173-1](../adr/2026-10-12-t2173-1-machine-mounts-only-run-and-clone-dirs-and-keeps-build-output-and-worker-cache-in-named-volumes.md)）。
判断は`domain::podman_machine`、use caseは`application::podman_machine`、podmanのコマンド・lock・`host.toml`の読み込みは`infrastructure::podman_machine`が持つ。

- 名前と接続: machineと接続の名前は`domain::podman_machine::MACHINE_NAME`で、podmanのコマンドはmachineの名前か`--connection`でそれを名指す。
  人の既定のmachineと既定の接続は変えず、作らず、止めない。
  Podmanの版によってはinitとstartが既定の接続を変えるので、`PodmanCli`が前の既定に戻す（前に既定が無ければ戻す先が無く、dagqの接続が既定に残りうる）。
- 起動と停止: `ensure`は無ければinitし、止まっていればstartし、使う者として数える入口で、workerのturnとverificationのcontainerの起動の前（runを使う者にする）とpodmanを要るtestとスモークの前に呼ぶ。
  使い終えたら`release`を呼び、使う者が居なければ止める。
  どちらもhost全体のlock（data homeの`dagq/machine/`）の中で判定と操作を行うので、並行するrun・verification・testが同時に呼んでも作るのと起動するのは1回になる。
  使う者は持ち主（`LeaseOwner`）が終われば数えず、呼んだprocessには依らない（turnとverificationの持ち主はrun）。
- 人のmachine: Podmanは同時に1つのmachineしか動かさないので、人のmachineが動いている間はdagqのmachineを起動せず、containerを使えないと答える（fail closed）。
  人のmachineは止めない。
- 資源: CPU・メモリ・diskはhost全体の`host.toml`の`[machine]`が与え、既定値と根拠は`MachineResources`のdoc commentが持つ。
  machineはhostに1つで全てのprojectが共有するので、queueの`host.toml`では変えない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定6）。
- メモリの予約: workerのturnとverificationのcontainerは、`ensure`の後、起動の前に同じlockの中でmemoryの上限をmachineのメモリから予約し、入らなければlockの外で待つ（[ADR-t3163-1](../adr/2026-10-25-t3163-1-containers-reserve-memory-on-the-machine.md)）。
  判断（予約の持ち方と待ちの順を含む）と値の根拠は`domain::machine_memory`が持つ。
  turnの予約はturnのcontainerに結び、verificationの予約は最初のコマンドの前から片付けまで持つ。
  新しいclaimは控え（[claimを控える](supervisor-lifecycle/claim-hold.md#workerを動かせないtask)）、次のturnはwrapperが、verificationは`integrate`が待ち、待ちはrunのevent（[計測](measurement.md#区間とタグ)）と`status`に出る。
  着地の再確認とfacts-onlyのturnも同じく予約し、wrapperは待つ間も終了の依頼と柵を読み、当たればturnを始めずに終わる。
- hostのmount: initのvolumeは各queueのrun dir・cloneの親のdir・verificationのcloneの親のdirだけで、hostの`$HOME`・workerのcache・buildの出力・着地用のworktreeのdirを含めない。
  各dirはmachineの中の同じpathにmountする。
  macOSのremoteのPodmanのbind mountのsourceはmachineの側のpathなので、containerに見せるhostのdirはmachineにmountしてある必要がある。
  queueのrun dir・runのclone・verificationのcloneはこの3つのdirの下に置き、着地用のworktreeはどの下にも置かない。
  initはhostの全てのqueue（data homeの`dagq/`の下でqueueのDBを持つdir）のdirをmountするので、作った時点のqueueがmachineを共有する。
  machineにmountしても、containerに見せるかはcontainerごとのmountで決まる。
- machineの中のnamed volume: buildの出力はrunかverificationごと、workerのcacheはqueueごとのnamed volumeで、hostにmountしない（`MachineVolumes`、名前は`QueueKey`）。
  名前がqueueとrunを含むので、あるqueueの操作は別のqueueのvolumeに届かない。
  runの終わりと着地でrunのbuildの出力を消すのは、relayの見張りが終わったrunを閉じる`RelayControl::close`で、runのcontainer・relay・socketのvolumeと一緒に消す（下の「brokerへの経路」）。
  machineを作り直すとvolumeは消え、workerのcacheは冷えた状態から始まる。
- 食い違いと作り直し: volumeはinitでしか決まらない。
  `ensure`と`MachineGate`は、queueのdirがmountされていないか、`dagq/`の下のqueueの3つのdirでないdir（`$HOME`、別の場所の同名のdir）がmountされていれば、作り直しの手順を添えて使えないと答える。
  作り直しは`podman machine stop dagq`と`podman machine rm dagq`で、次の`ensure`が今の`host.toml`とhostのqueueでinitする。
  作った後に足したqueueと資源の変更も、同じ手順で反映する。
- 使えるかの答え: `MachineGate`はlockを取らずmachineを変えずに見るだけで、無いか止まっているmachineは使えるとする（containerの起動の前の`ensure`が動かす）。

## image

containerのimageは、runtimeが持つbaseと、`dagq.toml`が名指すprojectのContainerfileの2段である（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定2）。
判断は`domain::container_image`、use caseは`application::container_image`、podmanのコマンドとmainのcheckoutの読み取りは`infrastructure::container_image`が持ち、queueの記録はsupervisorとwrapperではqueue serviceを通る（fail closed）。

- base: Containerfileは`src/infrastructure/container_image/base.Containerfile`に置き、binaryに埋め込む。
  Claude Code・Linuxのdagqのclient・非rootのuserだけを持ち、言語の道具を残さない。
  Claude Codeの版はruntimeが固定し（`CLAUDE_CODE_VERSION`）、新しい版はdagqの版と一緒に来る。
- client: installしたdagqと同じ版を、dagqのsourceからdagq専用のmachineの中でbuildし、baseに残さないstageで作ってbinaryだけを移す。
  releaseはcrates.ioのsource、開発版はbuild識別子が名指すcommitをdagqのrepositoryからcloneして取り、作ったclientの`--version`がinstallしたdagqのbuild識別子と同じでなければbuildを落とす。
  dagqのrepositoryにpushされていないcommitは取れず、baseのbuildが落ちる。
  hostにdagqのcheckoutは要らない。
- project: `dagq.toml`の`[container.image]`（綴りと意味は`ProjectImage`のdoc comment）が名指すContainerfileは、baseをbuildのARG（`DAGQ_BASE_IMAGE`）で受けてFROMにする。
  名指さなければbaseだけで動く。
  Containerfileと入力のファイルは制御側のmainのcheckoutから読み、runのcloneからは読まないので、着地前のworkerの変更でimageは変わらない。
  このrepositoryのものは`.dagq/container/Containerfile`である。
- tag: baseはruntimeのbuild識別子・Claude Codeの版・baseのContainerfile、projectはbaseのreference・Containerfileと入力のpathと中身から決まり、どれかが変われば新しいimageになる。
  projectのimageの名前はqueueを含み、あるqueueの片付けは別のqueueのprojectのimageに届かない。
  baseはhostの全てのqueueが共有し、新しいbaseを作ったqueueが他のbaseを消す（使われているものはpodmanが残す）。
- buildと片付け: `prepare`は、workerのturnとverificationの前にmachineを動かしてから、containerの起動の前に呼ぶ（下の「workerの実行」と「verificationの実行」）。
  `ImageGate`は見るだけである。
  無いimageを作り（baseが無ければprojectもその上に作り直す）、baseと同じqueueの古いtagを消す。
  containerが使うimageはpodmanが消させないので次の機会に残す。
  buildのcontextはimageのファイルだけを置いた新しいdirで、buildの引数は版とsourceだけなので、秘密情報とhostのpathはimageに入らない。
- 失敗: clientを入手できない、名指したファイルがmainのcheckoutに無い、buildが落ちたときは`container_image_failed`をqueueに記録する。
  `ImageGate`はそのimageの失敗から`FAILURE_HOLD_SECS`の間、理由つきで使えないと答え、時間が過ぎるか入力が変われば試し直させる。
  作ったときは`container_image_built`を記録する。

## brokerへの経路

runのcontainerは`--network none`で、外への経路はhostのqueueのbrokerへの1本だけにする（[ADR-t2114-3](../adr/2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)決定1〜4）。
`broker_relay`のmoduleは、判断が`domain`、use caseと門が`application`、relay・転送のprocessとpodmanのコマンドが`infrastructure`、見張りが`supervise::host_ops`の下にある。

- runのsocket: runのturnかverificationのcontainerを起動する前に、開いていなければ`application::broker_relay::open`が、machineの中にrunごとのnamed volumeを作ってそのrunのrelayを起動し、runの終わりに`close`がrelayとvolumeを消す。
  relayはrunのcontainerの非rootのuidで動き、そのcontainerと同じuidの対応（`keep-id`）でvolumeの中にsocketをmode 0600で作る。
  volumeをmountするのはrelayとそのrunのcontainerだけで、どちらも`:z`を付ける。
  開いたrelayは`<queue dir>/relay/`に記録するので、machineの停止で消えたrelayも見張りが見つける。
  記録は`close`が成功するまで残る。
- relay（`dagq relay serve`）: machineの中のcontainerで、socketへの接続をhostのbrokerのportへ双方向にそのまま流す。
  hostへはmachineのgvproxy（`host.containers.internal`）を通る。
  tokenを持たず、認証も宛ての判定もしない（brokerが行う）。
- 転送（`dagq relay forward`）: runのcontainerの中で`127.0.0.1`の決まったportを受け、socketへ流す。
  containerのenvがdagqのクライアントモードの宛て・Claude Codeの`ANTHROPIC_BASE_URL`（brokerの`/anthropic`）・標準のproxyのenvをこのportに向け、道具に固有の設定を持たない（[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)決定3）。
  run tokenをClaude Codeとproxyのenvに入れるのは、containerの中の`dagq relay turn`である（下の「workerの実行」）。
  クライアントモードの宛ては[Queue service](queue-service.md#クライアントモード)。
- DNSが無い: proxyのenvを見ない道具は名前を引けず外へ出られない。
  これは隔離の結果として扱い、要るものはhostのrunかqueue serviceのユースケースに回す。
- 見張り: supervisor（`--once`でないもの）は、brokerが答える間、各relayのcontainerが動くか・brokerの今のportへ向くか・socket越しにbrokerのhealthに届くかを見る。
  先に、記録のrunがqueueの上でもう生きていない（taskが進行中でない・後のrunに代わった・着地した・runが無い）relayは起動し直さず`close`で片付け、起動し直しの回数にもattentionの判定にも数えない。
  `close`が失敗した記録は次の回にやり直す。
  落ちたかportの古いrelayは起動し直す。
  動くのに届かないrelayがあるか、podmanがmachineに繋がらない（`--connection dagq`もgvproxyを通る）ときはgvproxyが落ちたとみて、machineをhost全体のlockの中で起動し直してから全てのrelayを起動し直す。
  relayを見るpodmanの呼び出しには時間の上限があり、超えたものは止めて繋がらないとして扱う。
  relayの起動（`open`と起動し直し）と`close`のpodmanの呼び出しにも時間の上限があり、見張りからのmachineの起動し直しのpodmanの呼び出しにはそれより長い別の上限がある。
  超えたものは止めて起動し直しか`close`の失敗として扱い、`close`が超えた回は残りの`close`を次の回に回す。
  回数の上限と窓はqueue serviceと同じ。
  起動し直せない・上限に達した・見られないときはinbox宛てのattention `queue_relay_down`を書き、全てのrelayが動けば消える。
  attentionが立つ間は`RelayGate`がcontainerを使えないと答え、新しいrunを控える（上の「fail closedの境界」）。

## git

containerで動くrunは、hostのgitの共通dirを共有しない独立したclone（`<queue dir>/clones/<run>`）で働く（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定1・3、[ADR-t2179-1](../adr/2026-10-12-t2179-1-control-side-runs-no-git-in-a-container-runs-clone-and-lands-in-its-own-worktree.md)）。
runのworktreeのpathにcloneを記録したrunがcontainerのrunで、hostのrunはworktreeのまま変わらない。
制御側の読み方と扱い方の分かれ目は`application::run_git`の1か所にあり、呼び出し元はcloneのpathをgitに渡さない。

- cloneの作成: provisioningで制御側が`file://`のURLでbase commitを取り込み（hard linkもalternatesも無い）、run branchをcheckoutし、cloneの`refs/remotes/dagq/main`と制御側のrun branchをbaseに置く。
  remote・credential helper・上流の資格情報は置かない。
- 事実: containerの中のexecutorがturnの終わりにrun dirへ書くrun branchのbundleと状態のファイルから読む。
  bundleの1本のrun branchを確かめて制御側のrun branchだけへ取り込む。
  読めない・食い違うものはsessionに返す（pollは書きかけとして待つ）。
- 事実だけのturn: 最後のturnの終わりのものでない事実は、headを読む前にagentもbrokerへの経路も無いturnで書き直す（[ADR-t2978-1](../adr/2026-10-23-t2978-1-facts-only-turn-runs-in-a-narrow-container-with-guarded-git.md)）。
  supervisorのpassから呼ばれるので、lock待ち・machineのstart・imageのbuild・`podman run`を10分（startと既存のimageとgitに足り、cold buildは拒む）、片付けを含め最大11分30秒に収める。
- 再計画の保留と復元: 書き出した作業と渡す材料による（[再計画](../plans/task-replanning.md)）。
- 着地・review・e2e: 制御側だけの`<queue dir>/landing/<run>`の着地用のworktree（着地する`result_commit`をdetachedで置く）で動く。
  どのmachineとcontainerのmountにも入らず、作れないときは着地せず人を待つ。
- main: rebaseを求めうる要求をsessionに渡す前に、制御側がmainのbundleをrun dirに書き、executorがcloneの`refs/remotes/dagq/main`へ取り込む。
  要求と差し戻しの理由はそのrefを名指す。
- hookとdriver: containerのrunのcommitを扱う制御側のgitは、hookを無いpathに向け、制御側のconfigの全scopeが定義するfilter・diff・mergeのdriverをプログラムを走らせないものに上書きして走る。
  着地用のworktreeで起こすプロセスにも同じ上書きをenvで渡し、上書きが効くと確かめられなければ着地しない。
- run dirのファイル: symlinkを辿らずに読み書きする。
- 片付け: worktreeと同じ契機で、clone・着地用のworktree・制御側のrun branch・run dirの事実と復元のファイルを、gitを使わずsymlinkを辿らずに消す。

## workerの実行

cloneで働くrunの各turn（最初のturn・revise・答え・resume）は、hostのprocessの代わりにdagq専用のmachineのcontainerで動く（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1、[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)決定1）。
session wrapperはhostの制御側に残り、turnの出力・receipt・askをrun dirから読み、`run log`で追える。
executorは`application::run_container::PodmanActorExecutor`、設定の読み取り・tokenの受け渡し・podmanは`RunContainers`のport、machine・image・relay・buildの出力の準備はhost運用が実行と着地に貸す`application::podman_machine::RunMachine`のport、`podman run`の引数・token・env・transcriptの置き場の判断は`domain::run_container`、containerの中の処理は`infrastructure::container_turn`（`dagq relay turn`）が持つ。

- 準備: 各turnの前に、machineを動かしてrunを使う者にし（`release`は呼ばない）、imageをbuildし、runのrelayを開き、`dagq.toml`が名指すvolumeを作り、`[run.env]`と`host.toml`のcontainerの上限を読む。
  どれかが失敗すればturnを起動せず、hostにも戻さない。
- token: supervisorが発行したworkerのtokenを、run dirのファイル（mode 0600、renameで置くのでworkerが置いたsymlinkを辿らない）にして、その場所をenvで渡す。
  podmanの引数とcontainerの設定にtokenの値は無く、ファイルはcontainerを消すときに消す。
- container: 非root（machineの中でmountの持ち主と同じuidの対応）・読み取り専用のroot filesystem（書けるのはmountと一時の`/tmp`だけ）・capabilityの削除・特権の昇格の禁止・memory/cpu/pidsの上限・`--network none`で、Podman / Dockerのsocketを渡さない。
  上限の既定と`host.toml`の`[container]`での上書きは`ContainerLimits`のdoc commentが持つ。
  mountはrunのclone・run dir・runのsocket・`dagq.toml`が名指すvolumeだけで、着地用のworktreeとverificationのcloneはmountしない（reviewの間も残るworkerのcontainerに、制御側が着地と検証に使う物を見せない）。
  podmanのcwdはrun dirで、制御側はcloneでプログラムを起動しない（上の「git」）。
- buildの出力とworkerのcache: `dagq.toml`の`[container.build_output]`が名指すdirにrunごとのvolumeを、`[container.worker_cache]`が挙げるdirごとにqueueのvolumeをmountする（綴りと意味は`run_env`の各表のdoc comment、案は[profile](toolchain-profiles.md#推奨の設定)）。
  runtimeは道具を知らず、projectが`[run.env]`で向ける。
  workerのcacheは信頼せず、同じqueueのworker・resumeだけが共有する（ADR-t2257-1決定5）。
- sccache: containerの中のserverを使う（[Run environment](supervisor-lifecycle/run-environment.md#sccacheのserver)）。
- containerの中: `dagq relay turn`がagentの前後の手順を持ち（順は`infrastructure::container_turn`のdoc comment）、agentが終われば結果に依らず、run dirの事実のファイルを上の「git」の形で書く。
  agentのhomeはrun dirの下に置き、次のturnがsessionをresumeできる。
- transcript: agentのhomeの`.claude`の下に、containerの中のcwd（clone）を名にしたprojectのdirでできる。
  sessionの有無の判断とmodel・コマンドの計測は、cloneのrunのworkerのsessionをhostに無ければそこで探す。
  どの段もsymlinkを辿らず、通常のファイルでない物や読めない物は無いものとする（ADR-t2179-1決定9）。
- 止める: turnの終わりごとにwrapperがcontainerを消す（podmanのprocessを止めてもcontainerは残るため）。
  止める経路での消去はbest effortで、machineが止まって消せなければlogに残し、runの終わりの片付けに任せる。
  runのsessionを止めるどの経路（reviewの後・着地・triageのstopとcancel・sweep・resumeの終わり）も`RecordingSessions`がcontainerを消し、復旧jobの`stop_processes`も消す。
  supervisorの再起動の後にadoptしたrunも、名前がrunから決まるので同じ経路で消える。

## verificationの実行

`dagq.toml`がverificationをcontainerにしたrunの着地では、rebaseの後の検証のコマンドを1本ずつ、そのverificationのcontainerで流す（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)決定1）。
結果・時間・失敗の分類の記録はhostと同じである。
`integrate`の`ContainerVerification`が流れを持ち、準備・podman・片付けは`application::run_container::VerificationContainers`のport、containerの中は`dagq relay verify`が持つ。
着地の再確認のcommandも同じ形で流す（[landing recheck](supervisor-lifecycle/integrate.md#landing-recheck)）。
`already_satisfied`のsnapshotの検証も同じ部品で、mainのheadのcloneで流す。
podmanがcontainerを起動しなかった（`podman_failed`）commandは判定せずhostで流さず、`integrate`は準備が落ちたときと同じくerrorで終える。

- 準備: machine・image・runのrelayをworkerのturnと同じ`RunMachine`で用意し、`dagq.toml`がbuildの出力のdirを名指せば、そのverificationごとの新しいvolumeを作る（workerのrunのvolumeと共有しない）。
- verificationのdir: 制御側だけのverificationのcloneの親のdirの下に、verificationごとの新しいdirを作る。
  dirのpathに既に物が在るか、親のdirが制御側だけのdirでなければ作らずに拒み、条件を満たす親のdirは複数のverificationが共有する（条件は`GitRepository::create_verification_clone`のdoc comment）。
  中にrebaseの後のcommitのclone（上の「git」の上書きでhookとdriverを止めた制御側のgitで作る）、tokenの写し（0600）、コマンドのhomeを置く。
  containerが始まった後は、制御側はこのcloneでgitを走らせない。
- 準備のコマンド: `dagq.toml`の`[container.verification]`が宣言したコマンド（`run_env::CONTAINER_VERIFICATION_TABLE`のdoc comment）を、制御側がmainのcheckoutからhostで流し、verificationのdirに材料を置かせる。
  runのコードは走らせず、宣言が無ければ流さず、落ちれば検証のコマンドを流さない。
  既知の失敗も書き、run dirの材料のdirへfdから写す（workerのsymlinkは辿らず置き換える）。
- container: workerのturnと同じhardeningと`--network none`で、mountはverificationのdir・run dirの材料のdir（読み取り専用）・runのsocket・そのverificationのbuildの出力のvolumeだけにする。
  着地用のworktree・workerのclone・run dirの他のファイル・workerのcache・hostのsupervisorのcacheはmountしない（[ADR-t2114-4](../adr/2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)決定4）。
  時間の上限はhostと同じく1本のコマンドに掛かり、上限で止めたcontainerは次のコマンドの前と終わりに消す。
- token: verificationのtokenを写しのファイルで渡し（podmanの引数に値は無い）、`dagq relay verify`が標準のproxyのenvに入れる。
  queueのbrokerはこのtokenを`dagq.toml`のregistryのallowlistへのtunnelにだけ通し、verificationの終わりに取り消す（[Queue broker](queue-broker.md)）。
- cache: `[run.env]`がsccacheを名指せば、保護されたcacheが無い今は`RUSTC_WRAPPER`を外してcacheなしでcompileし、`sccache_wrapper_removed`に残す。
- 片付け: verificationがどう終わっても、container・buildの出力のvolume・dirを消す。
  dirはgitを使わずfdから辿り、残ったcontainerがsymlinkに差し替えても外に届かない。
- hostの絶対pathを名指すverifyは、containerにそのpathが無いので落ち、hostに回さない。
  verifyはimageの`PATH`の道具を名前で名指し、要る道具はprojectのimageに入れる（[ADR-t2182-1](../adr/2026-10-14-t2182-1-macos-specific-tests-are-left-to-the-ci-macos-job-not-the-container-verification.md)決定3）。

## 後続の部品の置き場

次の部品が揃ったら、それぞれが`ContainerRuntime`の答えを埋める。
どれかが欠ければ使えないと答え、上の境界のまま控える。

- verificationの保護されたcache。
