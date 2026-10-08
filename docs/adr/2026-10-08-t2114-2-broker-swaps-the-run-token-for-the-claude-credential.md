---
id: adr-t2114-2
type: adr
title: containerにClaudeの資格情報を置かず、queueのbrokerがrun tokenを検証してclaude setup-tokenの長期tokenに差し替えてapi.anthropic.comに送り、前提がだめならAPI keyに替える（ADR-t1233-4決定4をamends）
status: accepted
created: 2026-10-08
updated: 2026-10-08
accepted_on: 2026-10-08
amends:
  - adr-t1233-4 decision 4
owners:
  - hisamekms
tags:
  - runtime
  - security
  - provider
related:
  - adr-t1233-1
  - adr-t1233-4
  - adr-t2114-1
  - adr-t2114-3
  - design-security
  - design-queue-service
---

# ADR-t2114-2: brokerがrun tokenをClaudeの長期tokenに差し替える

## Context

containerの中のworker（[ADR-t2113-3](2026-10-08-t2113-3-only-workers-resume-and-integrate-verification-run-in-containers.md)）のClaude CodeはClaudeのAPIを呼ぶ。
資格情報をcontainerに置けば、信頼しないコード（build.rs・proc macro・test）がそれを読んで持ち出せる。
今のhostのClaude Codeは人のkeychainのログインを使い、そのtokenはClaude Codeが自分でrefreshする。
同じログインを複数のプロセスが使うと、refreshのたびに古いtokenが失効して取り合いになる。

Claude Codeは`ANTHROPIC_BASE_URL`でAPIの宛先を、`ANTHROPIC_AUTH_TOKEN`で`Authorization`のbearerを替えられる。
`claude setup-token`は、refreshを要しない長期のtoken（サブスクリプションの認証）を発行する。
queueのbrokerは宛先を`api.anthropic.com`に限って認証を付ける（[ADR-t2114-1](2026-10-08-t2114-1-queue-broker-is-a-host-process-with-three-destinations.md)決定2）。

## Decision

1. **containerに資格情報を置かない。**
   containerのClaude Codeには、`ANTHROPIC_BASE_URL`にbrokerを、`ANTHROPIC_AUTH_TOKEN`にそのrunのrun token（[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4）を渡す。
   containerの中にkeychain・`~/.claude`の資格情報・長期token・API keyを置かない。
   [ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4はtokenの値をenvに置かずファイルの場所を渡すと決めたが、Claude Codeはbearerをenvでしか受けないので、containerのClaude Codeのprocessに限ってrun tokenをenvに置く（ADR-t1233-4決定4をamends）。
   supervisorはcontainerにtokenのファイルの場所を渡し、containerの中でClaude Codeを起動する処理がファイルを読んでenvに入れる（podmanの起動の引数とcontainerの設定にtokenの値を書かない）。
   envに置いてよい理由: envを避けた理由はcmuxのargvに出ることで、containerのworkerはcmuxを使わず、envはそのcontainerの中のprocessにしか見えず、tokenはそのrunの間だけ有効である。
2. **brokerがrun tokenを検証して差し替える。**
   brokerはrun tokenを検証し（失効・別のrunのもの・無いものは拒む）、`Authorization`を長期tokenに差し替えて`api.anthropic.com`に送り、応答（streamを含む）をそのまま返す。
   run tokenはbrokerの外（`api.anthropic.com`）へ送らない。
3. **長期tokenの置き場。**
   長期tokenは`claude setup-token`で人が発行し、hostの0600のファイルかkeychainに置き、queueのbrokerだけが読む。
   人のkeychainのClaude Codeのログインは使わない（refreshの取り合いになる）。
   tokenの発行・置き換えは人の操作で、AI actorには発行も読み取りもさせない。
4. **期限切れと認証切れは、今のClaudeの認証の控えのaskで扱う。**
   brokerが`api.anthropic.com`から認証の失敗を受けたら、そのまま実行側に返し、今のClaudeの認証切れと同じ分類と控え（新しいclaimを控え、人にaskで知らせる）に乗せる。
   brokerが勝手に別の資格情報に切り替えない。
5. **前提として確かめること。**
   次の3つは、実装の段で人かinboxが実tokenとhostで確かめる（workerは実tokenとhostを使えない。AGENTS.mdの「本番queueと開発環境の境界」）。
   1. サブスクリプションのtokenを、brokerが代理で付けてClaude Codeの要求を送ることが、規約の上で許され、動作の上で通ること。
   2. SSEのstream（Claude Codeの既定の応答）がbrokerを通って途切れずに返ること。
   3. 認証切れと利用上限の応答（statusとbodyの分類）が、broker越しでもClaude Codeと今の分類の処理から読めること。
6. **前提のどれかがだめなら、API keyに替える。**
   条件: 上の(1)が規約の上で許されないか、(1)〜(3)のどれかがbrokerの側の手当て（応答の素通し・headerの扱い）で直せないとき。
   手順の方針: 人がAnthropic ConsoleでこのqueueのためのAPI keyを発行し、長期tokenと同じ置き場（0600のファイルかkeychain、brokerだけが読む）に置き、brokerは`x-api-key`に差し替える。
   containerの側（`ANTHROPIC_BASE_URL`とrun token）は変えない。
   費用がサブスクリプションから従量に移るので、替えるかどうかは人が決め、決めたことをこのADRのamendsか置き換えで残す。

## Alternatives

- **containerに長期tokenかAPI keyを渡す**: 信頼しないコードが読んで持ち出せる。
  runごとに失効させられず、漏れたら人が発行し直すまで使える。
- **人のkeychainのログインをbrokerが使う**: Claude Codeのrefreshと取り合いになり、人のClaude Codeか全部のrunの認証が落ちる。
- **最初からAPI keyにする**: 前提の確認を待たずに費用の形が変わる。
  確かめて許されないと分かったときの替え先として残す。

## Consequences

- containerから持ち出せる資格は、そのrunの間だけ有効なrun tokenだけになり、brokerの宛先も3つに限られる。
- 長期tokenはhostの1か所にあり、漏れたときの影響はそのqueueの全部のrunに及ぶので、置き場の権限とbrokerだけが読むことを実装が確かめる。
- 前提の確認（決定5）が終わるまでこの決定の実装は着地させず、確認の結果はqueueのnoteかADRに残す。
