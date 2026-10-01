---
id: adr-t1233-5
type: adr
title: 読み取りのroleのjobとworkerは、DBのpathなしでqueue serviceの読み取りのユースケースでqueueを読み、roleごとに読める範囲はservice側のpolicyが決める。goal 82ではworkerを含む全roleに今の`queue.read`と同じqueue全体の読み取りを許し、Codexのsandboxの中のworkerとjobもserviceに届くようにする（ADR-t813-3決定3をamends）
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amends:
  - adr-t813-3 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - security
  - provider
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t728-1
  - adr-t813-3
  - adr-t1063-1
  - adr-t980-1
  - design-security
  - design-authorization
---

# ADR-t1233-5: 読み取りのユースケースとroleごとの読める範囲、workerの読める範囲、Codexのsandboxからの到達（ADR-t813-3決定3をamends）

## Context

[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)の段(3)は、workerとjobのプロセスからqueue DBのpathを外す。今、review・plan review・goal review・復旧・スループットの見直しのjobとobserverは、promptとskillの指示で`events`・`timeline`・`stats`・`kpi`・`marks`・`search`・`related`・`findings`・`goal show`・任意のtaskとrunの`show`などを打ってqueueを読む。

[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)のpolicyでは読み取り（`queue.read`）は全roleが持ち、workerもqueue全体を読める（[Authorization](../design/authorization.md)）。measureのtaskのworkerは本番のqueueを`stats --full`・`events --full`・`timeline`・`kpi`・`marks`・`forecast`などで読み、結果をdocsに書く（commit 2ae2c673、task 1205・1114・1034・1026・601・1200、AGENTS.mdの「テストの制約」のchangeの`measure`）。一方workerのprompt（`WORKER_READING`）は`dagq list`と`dagq show`を打たないよう言う。

Codexのworkerはworkspace-writeのsandboxでnetworkを開けて動き、queue dirを書けないので、`dagq ask`はrun dirへの要求にしてsupervisorが取り込む（[ADR-t813-3](2026-09-28-t813-3-codex-worker-permissions.md)決定2〜4）。Codexのjobは読み取りだけのsandboxで動く（[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)決定2）。goal 80のCodexのobserverはfindingを書く経路を要る。

## Decision

1. **読み取りはserviceの読み取りのユースケースで提供する。** promptとskillが読み取りのroleのjobとworkerに打たせる読み取りのコマンドは、クライアントモードでserviceの読み取りのユースケースに写し、出力の形は変えない。DBのpathを持たないプロセスがqueueを読むのはこの経路だけで、読み取りのためにDBのファイルを実行側に見せない。
2. **roleごとに読める範囲はservice側のpolicyが決める。** 書き込みと同じく、読み取りもprincipal（ADR-t1233-4のtoken）からserviceがADR-t728-1のdefault denyの静的なpolicyで判定する。クライアントが読む範囲を自分で狭めることに頼らない。`watch`と書き出し（`queue.export`）の今の拒否もservice側で保つ。
3. **goal 82では読み取りの範囲を狭めない。** workerを含む全roleに、今の`queue.read`と同じqueue全体の読み取り（measureのtaskが使う`stats`・`events`・`timeline`・`kpi`・`marks`・`forecast`などを含む）を、serviceの読み取りのユースケースで許す。読み取りは状態を変えず、狭めるとmeasureのtaskの手順とAGENTS.mdの規則が本番で止まるため。workerのpromptが作業の初めに`list`と`show`を打たないよう言うのは、作業の初めに何を読むかの指示で、権限の範囲ではない。
4. **Codexのsandboxの中からもserviceに届く。** Codexのworker（workspace-write、networkを開ける。ADR-t813-3決定4）とjob（読み取りだけ）の中のクライアントモードのdagqが、hostのserviceのsocketに届くように、providerの実装が起動の引数でsandboxからそのsocketに届くようにする。workerはnetworkを開けたまま（ADR-t813-3決定4を狭めも広げもしない）で、socketへの接続はその中で届く。読み取りだけのjobはnetworkを開けず、そのsocketへの接続だけを足して許す。これはADR-t1063-1決定2の「権限を意図で渡し、providerの実装が訳す」の意図（dagqの読み取りを打てる）の訳し方で、決定2を変えない。queue dirとDBのファイルへの書き込みは今までどおり許さない。sandboxがsocketだけを許せないなら、段(4)のqueueのbrokerと同じloopbackのTCPとtokenに替え、jobにはそのloopbackの宛先だけを許す。どちらで届けるかは実装のtaskが確かめて`docs/design/`に書く。
5. **ADR-t813-3決定3をamendsする。** Codexのworkerがqueueに書く操作（`dagq ask`）は、クライアントモードではrun dirへの要求ではなく、serviceのユースケースとして送り、serviceがprincipalで認可する。queue dirを書かせないこと（決定3の理由）は変わらない。run dirへの要求をsupervisorが取り込む経路は、クライアントモードが着地するまでの間だけ使う。observerなどqueueに書くjobをCodexに乗せるとき（ADR-t1063-1決定3・7）も、同じservice経由の経路で書く。

読み取りのユースケースの一覧と名前、sandboxに渡す設定、APIの形は実装のtaskが`docs/design/`に書く。

## Alternatives

- **workerの読み取りを自分のtaskとrunに狭める**: 読み取りの漏れを減らせるが、measureのtaskが本番のqueueを読めなくなり、誰がqueueを読むか（measureの手順）を合わせて変える必要がある。隔離（goal 38の段(4)〜(6)）で読み取りの漏れが問題になったときに、measureの手順と合わせて別のADRで決め直す。
- **読み取りのroleのjobにはDBを読み取り専用で見せる**: schemaの互換とpathの露出が残り、コンテナに移すときにmountが要る（ADR-t1233-1のAlternatives）。
- **jobに要る読み取りをsupervisorがpromptに埋めて渡す**: promptが大きくなり、jobが調べながら読む（`search`・`related`・`timeline`）ことができない。
- **Codexのworkerのrun dirへの要求を続ける**: 書く操作ごとにsupervisorの取り込みの形を足し、askの開く時刻がsupervisorのpassまで遅れる。serviceがあればprincipalで直接認可できる。

## Consequences

- 段(3)の後も、jobとworkerのpromptとskillの読み取りのコマンドは変えずに動く。measureのtaskの手順も変わらない。
- workerは他のrunやtaskの記録を読めるままなので、prompt injectionやreceiptを通じた読み取りの漏れの面は今と同じで、隔離の段で改めて扱う。
- Codexのsandboxの設定に、serviceへの到達の分が加わる。
