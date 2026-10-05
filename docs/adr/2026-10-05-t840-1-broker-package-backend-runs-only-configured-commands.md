---
id: adr-t840-1
type: adr
title: brokerに4つめのbackendのpackageとcapabilityのpackage.installを足し、repositoryが名前ごとに設定した少数のコマンドだけを、名前で選ばせてprocess.execと同じ閉じ込めと上限で走らせる
status: accepted
created: 2026-10-05
updated: 2026-10-05
accepted_on: 2026-10-05
amends:
  - adr-t827-2 decision 1
  - adr-t827-4 decision 1
  - adr-t827-4 decision 4
  - adr-t827-4 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - security
  - broker
related:
  - adr-t827-2
  - adr-t827-4
  - adr-t838-1
  - design-broker
---

# ADR-t840-1: brokerに4つめのbackendのpackageとcapabilityのpackage.installを足し、repositoryが名前ごとに設定した少数のコマンドだけを、名前で選ばせてprocess.execと同じ閉じ込めと上限で走らせる

## Context

[ADR-t827-2](2026-09-28-t827-2-broker-transport-run-token-and-workspace-confinement.md)決定1はbrokerのbackendを`fs`・`process`・`git`の3つに限り、[ADR-t827-4](2026-09-28-t827-4-worker-mcp-tools-audit-mode-and-relations.md)決定1はworkerの道具をfs・`process.exec`・run branchのgitに、決定4は`dagq.toml`の`[broker]`に置く方針をmodeとexecのallowlistと上限に、決定5はbrokerのcapabilityをfs・process・gitのopに限った。

goal 59（2026-09-27の人の計画のPhase 2）は計画書の1.9として、依存の取得（`cargo fetch`・`npm install`など）をbroker経由で行うpackage backendを求める。`process.exec`のallowlistは`argv[0]`だけを見るので、`npm`を入れれば任意の引数で走らせられ、依存の取得だけを許すことができない。`required`（[ADR-t838-1](2026-10-05-t838-1-required-broker-mode-refuses-built-in-tools-and-holds-claims.md)）のworkerは組み込みのBashで依存を取れないので、broker経由の道が要る。

## Decision

1. ADR-t827-2の決定1を変える。backendは`fs`・`process`・`git`・`package`の4つにする。pathの形`/v1/<backend>/<op>`、127.0.0.1だけのHTTP+JSON、7つのerror code、未知のop・欄を`invalid_request`で拒むことは変えない。
2. ADR-t827-4の決定1を変える。workerの道具に、repositoryが設定したpackageのコマンドを名前で選んで走らせる道具を足す。workerはargv・env・stdinを渡せず、選べるのはどのコマンドかだけにする。
3. ADR-t827-4の決定4を変える。repositoryの方針として`dagq.toml`の`[broker]`に置くものに、packageのコマンド（名前ごとのargv）を足す。`host.toml`との分け方とmodeの落とし方は変えない。
4. ADR-t827-4の決定5を変える。brokerのcapabilityに`package.install`を足す。`process.exec`とは別のcapabilityにし、tokenがどちらか一方だけを与えられるようにする。dagqのroleからの写しで与えること、queueの`Capability`と別の名前空間であること、予約のcapabilityを与えないことは変えない。
5. 設定に無い名前は`capability_denied`で拒む。プログラムはpathでなく名前で、`git`は設定でも拒む（gitはgitのbackendだけが走らせる。ADR-t827-2決定7）。
6. 走らせ方は`process.exec`と同じにする。shellを通さず、workspaceをcwdにし、envは空から始めてbrokerとrequestのenvを継がず、timeoutと出力の上限はserverが強制し（ADR-t827-2決定8）、超えればprocess groupを止める。auditは`process.exec`と同じく、引数と出力を残さない。

## Alternatives

- **`process.exec`のallowlistにpackage managerを入れる**: `argv[0]`しか見ないので、`npm`を許せば`npm exec`や任意のscriptも許す。依存の取得だけに絞れない。
- **requestにargvを持たせ、serverがprefixで照合する**: 引数の並べ方とoptionの意味がtoolごとに違い、照合を抜ける形を閉じきれない。名前で選ばせ、argvは設定だけが持つ。
- **networkの制限も今決める**: backendごとのegressとcredentialの分離はPhase 4（goal 59の範囲外、人の決定 2026-09-27）。この決定は走らせるコマンドを限ることだけ。

## Consequences

- 限るのはargvだけで、コマンドが読むworkspaceの中身（workerが書ける）は限らない。`npm install`のlifecycle scriptなど、設定したコマンドがworkspaceのものを走らせうる。誤りを止める仕組みで境界ではない（ADR-t728-1の助言的なhost、ADR-t827-4決定5）。scriptを走らせない形の設定を勧める。
- brokerのimageにはtoolchainが無いので、containerで実際に`cargo`・`npm`を走らせるにはimageかmountの用意が別に要る。
- capabilityの名前が1つ増えるので、古いbuildのbrokerは新しいtokenを読めない。版が食い違えばbrokerを使わないこと（ADR-t827-1）で守る。
- 設定のkey・flag・requestの形・道具の名前・auditの欄・設定の検査は[Resource broker](../design/broker.md)の「package.install」が持つ。
