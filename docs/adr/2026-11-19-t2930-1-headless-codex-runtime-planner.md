---
id: adr-t2930-1
type: adr
title: 非対話のruntimeのplannerをCodexで動かし、読み取り専用のsandboxからqueue serviceを使い、起動時に使えないproviderから切り替える
status: accepted
created: 2026-11-19
updated: 2026-11-19
accepted_on: 2026-11-19
amends:
  - adr-t1394-2 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - provider
  - security
related:
  - adr-t1222-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t1063-1
  - adr-t1433-2
  - adr-t1704-1
  - adr-t1857-1
  - adr-t3102-1
---

# ADR-t2930-1: 非対話のruntimeのplannerをCodexで動かす

## Context

[ADR-t1394-2](2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)で作った非対話のturnの経路を、人は評価の後も使い続けると決めた。
[ADR-t1433-2](2026-10-03-t1433-2-abolish-the-interactive-route.md)によりruntimeのplannerはbackgroundのwrapperで動く。
現在はClaudeだけがplannerのturnを実行でき、`--no-claude`のhostではplannerを立てない。
Codexのworkerは作業用worktreeに書く権限を持つが、plannerに必要なのはrepositoryを読んで計画をqueueに記録する権限である。

## Decision

1. **plannerの操作はqueue serviceで認可する。** Codexのplanner内の`dagq`はクライアントモードで動き、`add`・`edit`・`submit`・`ask`・`note`・`judge-follow-up`などをplannerのユースケースとして送る。
   [ADR-t1222-1](2026-10-02-t1222-1-codex-observer-writes-through-the-queue-service.md)のobserverと同様に、serviceは[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)のtokenからprincipalを認証し、plannerのroleとそのplannerのIDで認可と記録の帰属を決める。
   Claudeのplannerが既に使う[ADR-t3102-1](2026-10-22-t3102-1-stage-5-principals-user-by-socket-peer-and-control-side-tokens.md)の発行経路に揃え、plannerを開く側が発行を依頼したtokenのファイルをturnに渡し、plannerが閉じると資格を失効させる。
   wrapperの資格は分けて保持する。serviceに届かない操作はクライアントのエラーとして返す。

2. **Codexのturnはrepositoryのcheckoutのrootで読み取り専用のsandboxを使う。** agentのコマンドにはrepositoryの文書とコード、当該plannerへの受け渡しファイル、指示の参照先、CLIとその実行に必要なファイル、当該plannerの資格だけを読ませる。
   checkoutを読む権限からqueueのDB、人の資格、他のsessionの領域は除き、repository・plannerのdir・queueには直接書けないようにする。
   [ADR-t1233-5](2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)のとおり、queueの読み取りはserviceの読み取りのユースケースとroleのpolicyが決め、ファイルの読み取りはCodexのsandboxが制限する。
   `job_service_config`と同じくqueue serviceのUnix socketだけを許し、agentのコマンドの外部networkは閉じる。最初のturnとresumeの両方に同じ制約を渡す。

3. **plannerの指示は起動側が初回promptに載せる。** Claudeにpluginで渡している`dagq-planner` skillを共通の指示元とし、Codexにはruntimeがその内容を初回promptに組み込む。
   skillにあるClaude固有の変数は実際に使う`dagq`と参照文書の場所に解決し、参照先はsandboxから読めるようにする。
   Codexの個人設定やskillの自動発見に頼らず、各turnの環境に`DAGQ_ROLE=planner`を置き、初回promptにはClaudeと同じrepositoryの言語の指示を付ける。

4. **goal 87のturnの駆動を共用する。** `send_to_planner`・`write_request`・idle marker・`turn::request_to_take`を使い、Codex用の配送の仕組みは作らない。
   最初のturnは`codex exec`で始める。runtimeの配送で`codex exec resume`を使うのは、生きているplannerへのrevise・answer・`provider retry`に限り、同じthreadに届ける。
   終了は今のwrapperの経路を保つ。`send_to_planner`の`Input::Exit`が書く`turns/exit`を`Turns::next_request`が読み、agentを呼ばずに`None`を返してwrapperが終わる。終了のためのresumeやCodexの追加呼び出しは行わない。
   人の答えだけを待って閉じたplannerへのanswerは、[ADR-t1704-1](2026-10-05-t1704-1-human-answer-wait-releases-runtime-planner-slots.md)のとおり、新しいplannerの初回promptで記録とともに引き継ぐ。閉じたplannerのthreadはresumeしない。
   `REVISE_ESCALATION`はreviseのために新しく立てるときに適用し、選んだCodexのmodelを保ったまま、設定または既定のeffortを一段上げてCodexのreasoning effortに写す（`low` → `medium` → `high` → `xhigh`が上限。`max`は`xhigh`に写す）。生きているplannerにreviseを届けるときはmodelとeffortを保つ。

5. **起動では使えるproviderを選び、開始済みのsessionは同じproviderで続ける。** これを[ADR-t1394-2](2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)決定5の読み替えとする。
   `[roles.runtime_planner]`でproviderを選ぶ。起動時に明示したproviderが使えない場合の切り替えは、他のheadless jobと同じ分類と`[provider_fallback] jobs`に従う（[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)決定4・5、[ADR-t1857-1](2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)）。
   実行ファイルの欠如・起動不能・認証・利用上限でsessionを開始できず、すでに引き継ぐthreadも実行済みのplanner操作も無いときだけ、起動時の切り替えを行う。Codexが使えなければ許可されているClaudeで、Claudeが使えなければ許可されているCodexで、切り替え先の既定のmodelとeffortを基準に同じ仕事を始め、理由を記録する。通常のturnの失敗は切り替えの理由にしない。
   開始済みのsessionが壁に当たったらproviderごとの控えを待ち、解けたら`provider retry`で失敗した依頼を先に届ける。待ちは決めずに終わった回数に数えない。
   `--no-claude`ではClaudeの起動を禁止し、Codexが使えればplannerを立てる。明示的な禁止によるCodexの選択は`jobs = false`でも行う。
   使える起動先が無ければplannerを立てず、依頼とreviseを待ちに残し、理由をinboxから確認できるようにする。使えるproviderが戻れば待たせた仕事を引き取る。

6. **区間と起動の記録は実際に動いたproviderとmodelを持つ。** plannerの区間と`session_opened`などの起動の記録に実際のproviderと判明したmodelを残す。実際のmodelが分からない場合は要求modelと未確認であることを区別して残す。
   稼働時間・tokenは両providerとも既存のturnの記録から取り、切り替え前後を実際のproviderとmodelで分けて比べられるようにする。

## Alternatives

- **従来のClaude plannerのようにqueueのDBへ直接書く。** CodexのsandboxにDBとその周辺への書き込みを許すと、plannerのユースケースに限るserviceの認可を経ずにqueueの状態を変えられる。現行のClaude plannerもservice経由なので、直接書く経路を設けるとprovider間で認可と記録の経路が分かれる。
- **sessionの途中でproviderを切り替える。** 別のproviderは元のthreadをresumeできず、進行中の判断を引き継ぐ別の仕組みが要る。書き込み済みの操作を繰り返すおそれもあるため、同じsessionの控えと再送で続ける。
- **Codex専用のplanner skillを持つ。** 判断の基準とCLIの手順を二重に保守する必要が生じる。共通の指示をruntimeから渡せば、初回の起動に必要な差だけを吸収できる。

## Consequences

- 実装するtaskは、turn・手渡し・待ちの流れを[`plan-planners.md`](../design/supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)、providerの選択・effort・記録を[`actor-model.md`](../design/supervisor-lifecycle/actor-model.md#provider)、sandboxとroleの読み取りの境界を[`security.md`](../design/security.md)に反映する。
- このrepositoryでproviderを切り替えるときは印を記録し、その前後を決定6の区間で比較する。
