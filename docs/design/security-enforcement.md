---
id: design-security-enforcement
type: design
title: 強制の境界と道筋
status: current
created: 2026-11-08
scope: runtime
tags:
  - security
related:
  - design-security
  - design-container-runtime
  - design-queue-broker
  - adr-t728-1
  - adr-t728-2
  - adr-t728-3
---

# 強制の境界と道筋

[Security](security.md)の信頼と権限の模型を、実行環境で強制するときの境界。
capabilityの模型はbackendを替えても変えず、sandboxやserviceは同じ模型の上に強制の点を足す（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)の決定6）。
模型の定義を知りたいときは`domain::authorization`を入口にする。
どの境界も、その大きさで守れないときは弱めず、follow-upのtaskにする。

## container

隔離の適用先とfail closedの判断は[Container runtime](container-runtime.md#fail-closedの境界)を入口にする。
workerの全turnと着地のverificationは適用先ごとにhostかcontainerを選び、containerを使えない適用先は控えてattentionで知らせ、hostに黙って戻さない。
containerはrunのcloneとrun dirだけをmountし、queue DB・main checkout・他のrunを見せず、enforcementはsandboxになる（mountの例外は[Container runtime](container-runtime.md)）。
machineにもhostのホームをmountしない（[dagq専用のmachine](container-runtime.md#dagq専用のmachine)）。
予約のcapabilityを強制するのは隔離のbackendである。
containerで動かすのは信頼しないコードを走らせるworker・resumeと着地のverificationだけで、読むだけのheadlessのjobの隔離はOSのsandboxにする（[ADR-t2113-3](../adr/2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）。
containerのrunにはhostのgitの共通dirをmountせず、runごとのcloneを渡す（[ADR-t2113-2](../adr/2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)）。

## queue serviceとbroker

DBの直接操作とenvの偽装を塞ぐには、AI actorがqueueのファイルに触れず、制御側のserviceにだけ依頼する形にする。
DBを開ける境界は[Queue service](queue-service.md#dbを開く経路)、principalの認証は`QueueService::authenticate`を入口にする。
serviceは発行した資格かsocketのpeerでactorを識別し、processのroleの環境変数を信用しない（[ADR-t3102-1](../adr/2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)）。
host構成ではtokenのfileもDBも同じユーザーが読めるので、助言的であることは変わらない（[ADR-t1233-4](../adr/2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定5・ADR-t3102-1決定5）。
認可は同じapplicationの境界で行い、wrapperとhookはworkerの環境から分けたwrapperのprincipalで判定する。

外へ出られる宛先の限定と資格情報をcontainerに置かない保証は[Queue broker](queue-broker.md#不変条件)を入口にする。
brokerはqueue serviceと同じ起動・停止・execの引き継ぎで動くhostのプロセスである（[ADR-t2114-1](../adr/2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)）。
宛先の設定をruntimeに固定しない約束は[ADR-t2257-1](../adr/2026-10-12-t2257-1-container-runtime-takes-project-and-host-values-from-config.md)、run tokenとhostの資格情報の差し替えは[ADR-t2177-1](../adr/2026-10-12-t2177-1-broker-swaps-the-run-token-for-a-claude-credential-chosen-in-host-toml.md)が持つ。
containerからbrokerへの唯一の経路と、relayが認証も宛ての判定もしない境界は[Container runtime](container-runtime.md#brokerへの経路)を入口にする。

ここでいうbrokerはqueueのbrokerだけを指す。
resource brokerは置かず、実行側は隔離環境の中で組み込みの道具をそのまま使い、境界は隔離環境が持つ（[ADR-t2113-1](../adr/2026-10-08-t2113-1-remove-the-resource-broker.md)）。

## Integratorの分離

pushの資格情報をIntegratorのプロセスだけに持たせ、supervisorとAI actorから外す。
現在の論理的な境界は[SecurityのreviewのpassとIntegrator](security.md#reviewのpassとintegrator)を入口にする。

## 人しか出せない承認

人だけに限る強制は、承認の経路（別のterminal、署名、人の端末からの確認など）を決めてから行う（[ADR-t728-3](../adr/2026-09-27-t728-3-answer-and-delegated-authority-of-the-inbox.md)決定3）。
現在の代行の記録と残るリスクは[Security](security.md#answerと代行の記録)を入口にする。
