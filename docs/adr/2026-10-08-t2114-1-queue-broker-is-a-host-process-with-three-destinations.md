---
id: adr-t2114-1
type: adr
title: queueのbrokerはhostのプロセス（制御側）としてqueue serviceと同じup / down / execの引き継ぎで動かし、宛先をqueue service・api.anthropic.com・crates.ioの3つに限る（ADR-t1233-1決定6をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1233-1 decision 6
owners:
  - hisamekms
tags:
  - runtime
  - security
  - architecture
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t2113-1
  - adr-t2113-3
  - adr-t2114-2
  - adr-t2114-3
  - adr-t2114-4
  - design-queue-service
  - design-security
---

# ADR-t2114-1: queueのbrokerはhostのプロセスとして動かし、宛先を3つに限る

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定6は、queueのbrokerを実行側から制御側への唯一の出口にし、run・jobごとのtokenを認証してprincipalを付けてqueue serviceに転送し、egressのallowlist（crates.io・GitHubなど）も兼ねると決めた。
どこでbrokerを動かすか、宛先を何に限るかは決めていなかった。
[ADR-t2113-1](2026-10-08-t2113-1-remove-the-resource-broker.md)決定3はresource brokerを外し、隔離の外へ出る経路をqueueのbrokerだけにすると決め、その形を後続のADRに回した。
[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)は、podmanを使うのをworker・resumeとintegrateのverificationだけにし、制御側はhostに置くと決めた。

containerの中のworkerが外へ出る必要があるのは、queueの操作（`dagq ask`・`show`・`note`など）、ClaudeのAPI、cargoの依存の取得である。
GitHubのCIの読み取り（`dagq ci failures`など）を要したrunは、直近300 runで約14件だった。

## Decision

1. **brokerはhostのプロセス（制御側）で、podmanでは動かさない。**
   brokerはqueueごとに1つで、queue serviceと同じ起動・停止の手順に乗せる（[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定1・2）。
   `up`がsupervisorより先に起動し（生きていればreuse、buildが違えば入れ替え）、`down`がsupervisorの後に止め、`install`と自動更新のexecの引き継ぎで新しいバイナリに入れ替え、生きている間はsupervisorが見張って起動し直す。
   固定バイナリだけで動き、開発中のバイナリは本番のbrokerを起動しない。
2. **宛先は次の3つに限り、allowlistの外は拒む。**
   - queue service: 要求のrun tokenを検証し、principalを付けてserviceに転送する（ADR-t1233-4決定4のtokenをそのまま使う）。
   brokerが受けるtokenは、制御側がworker・resumeのrunとintegrateのverificationごとに発行したものだけである。
   verificationのtokenはそのrunのverificationをprincipalにし、届く宛先はcrates.ioだけにする（queue serviceとClaudeのAPIは使わない）。
   - `api.anthropic.com`: ClaudeのAPIの要求に、brokerが認証を付けて送る（[ADR-t2114-2](2026-10-08-t2114-2-broker-swaps-the-run-token-for-the-claude-credential.md)）。
   - `crates.io`（cargoの依存の取得に要るhostを含む）: HTTPの`CONNECT`をallowlistのhostにだけ通す。
     allowlistに足すhostは実装がdesignに書き、足すときはこの決定の宛先の範囲（cargoの依存の取得）に収まるものに限る。
   拒んだ要求はrunとhostをeventに残す。
3. **GitHubのCIの読み取りはbrokerの宛先にしない。**
   要るなら、queue serviceのユースケース（制御側がGitHubを読み、結果だけを返す）にする案とし、頻度が低い（Contextの約14件）ので後回しでよい。
   それまでcontainerのrunはGitHubに届かない。
4. **sccacheはbrokerの宛先にしない。**
   containerの中のcompileの経路は[ADR-t2114-4](2026-10-08-t2114-4-sccache-server-inside-the-run-container.md)が決める。
5. **ADR-t1233-1決定6を変える範囲と保つ範囲。**
   変えるのは、egressのallowlistの中身（GitHubを外し、上の3つに限る）と、brokerの置き場をhostのプロセスと決めることである。
   containerからbrokerへの経路（決定6の「loopbackのTCPとtoken」）は[ADR-t2114-3](2026-10-08-t2114-3-worker-containers-reach-the-broker-through-a-unix-socket-relay.md)が変える。
   保つのは、brokerが実行側から制御側と外への唯一の出口であること、tokenで認証してprincipalを付けること、resource brokerと別物であることである。

## Alternatives

- **brokerをpodmanのcontainerで動かす**: brokerは長期のClaudeの資格情報（ADR-t2114-2）とqueue serviceへの経路を持つ制御側で、実行側と同じmachineに置くと、machineのVMの境界の内側に秘密が入る。
  起動と停止もqueue serviceと別の手順になり、machineが止まると全部のrunが外へ出られなくなる。
- **GitHubをCONNECTのallowlistに入れる**: GitHubは上流のgitと同じhostで、pushの経路と読み取りをhostの名前で分けられない。
  実行側からpushへ届く経路を持たない（[ADR-t2113-2](2026-10-08-t2113-2-git-is-guarded-by-the-mount-design.md)決定3）ことを、brokerのhostの判定で保つことになる。
- **宛先を限らず、denylistで危ないhostだけを拒む**: 信頼しないコードが任意の外のhostへ出られ、隔離の意味が無くなる。
- **brokerとqueue serviceを1つにする**: ADR-t1233-1のAlternativesのとおり、hostの呼び出し元と実行側では認証と露出が違う。
  起動と停止の手順は揃えるが、プロセスは分ける。

## Consequences

- containerのrunは、queue・ClaudeのAPI・cargoの依存の取得の3つだけで外へ出る。
  `gh`・GitHubのAPI・ほかのpackage registryは使えず、要るtaskはhostのrunか、queue serviceのユースケースを足す後続に回る。
- brokerが落ちるとcontainerのrunは外へ出られない。
  queue serviceと同じく、supervisorが見張って起動し直し、起動できなければattentionで人に知らせる形を実装が足す。
- brokerの実装（goal 38の段(4)）までは、今のhost構成のままで、この決定の宛先の制限は効かない。
