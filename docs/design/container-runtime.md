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
  部品が揃うまでの実装`infrastructure::container_runtime::UnbuiltContainer`は、常に理由つきで使えないと答える。
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

## 後続の部品の置き場

次の部品が揃ったら、それぞれが`ContainerRuntime`の答えを埋める。
どれかが欠ければ使えないと答え、上の境界のまま控える。

- dagq専用のpodman machineとcontainer image（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)決定4〜6）。
- hostのqueueのbroker（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）と長期tokenの差し替え（[ADR-t2114-2](../adr/2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)）。
- VMの中のrelayとrunごとのunix socket（[ADR-t2114-3](../adr/2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)）。
- runごとのcloneとbundleの取り込み（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）、containerの中のsccache（[ADR-t2114-4](../adr/2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)）。
- worker・resumeとverificationのcontainerの実行（`ExecutorBackend::Container`の実装）。
