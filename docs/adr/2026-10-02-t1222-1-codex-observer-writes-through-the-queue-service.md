---
id: adr-t1222-1
type: adr
title: Codexのobserverは、jobのdagqをクライアントモードにしてqueue service経由でfindingの記録・更新・resolveとfindingに紐づくblockedのaskを書き、書ける範囲はservice側がobserverのroleに限り、記録の帰属は今のobserverのまま保ち、Claudeのobserverも同じ経路にし、Claudeのobserverの許す道具の代わりはread-onlyのsandboxとservice側の認可が果たす（ADR-t1063-1決定2・3をamends）
status: accepted
created: 2026-10-02
updated: 2026-10-02
accepted_on: 2026-10-02
amends:
  - adr-t1063-1 decision 2
  - adr-t1063-1 decision 3
owners:
  - hisamekms
tags:
  - runtime
  - provider
  - observer
  - security
related:
  - adr-t1063-1
  - adr-t1233-1
  - adr-t1233-4
  - adr-t1233-5
  - adr-t813-3
  - adr-t728-1
  - adr-0044
  - adr-t1091-1
  - design-supervisor-lifecycle-observer
  - design-supervisor-lifecycle-actor-model
  - design-authorization
---

# ADR-t1222-1: Codexのobserverはqueue service経由で書く（ADR-t1063-1決定2・3をamends）

## Context

[ADR-t1063-1](2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)はheadlessのjobのproviderを役割ごとに選べるようにし、決定2で「queueに書く役割（observerなど）のsandboxは、それを乗せるtaskが決定3とともに決め直す」、決定3で「sandboxが代わりを果たさない役割（queueに書くobserverなど）をCodexに乗せるときは、そのtaskが扱いを決め直す」と委ねた。goal 80はobserverをCodexに乗せる。

今のobserver（[Observer](../design/supervisor-lifecycle/observer.md)の2・5と権限）は、agent自身が`DAGQ_ROLE=observer`のenvで`dagq finding record` / `finding resolve`と`dagq ask --kind blocked --finding`を打ってqueueに書く。CLIのpolicy（[ADR-t728-1](2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)）がobserverにはこの3つと読み取りだけを許す。同じ種類・対象・subjectの既存のfindingへの合流、何も新しくない記録は何も書かないこと、根拠のevent idの検査は、`finding record`のqueueの側（findingを記録する1つのtransaction）が行う。終わった後に`dagq observe`がpayloadの`by: observer`の`finding_recorded` / `finding_updated` / `finding_status_changed`と`asked_by: observer`の`ask_opened`を集めて`observe_finished`に件数とidを書き、次のobservationを起こすかの判定（observer.mdの0）はそれらをobserver自身のeventとして数えない。Claudeのobserverはdagqのコマンドだけを許す道具とし、hookと拒むコマンドの一覧の設定は持たない。

Codexのheadlessのjobはread-onlyのsandboxで動き（ADR-t1063-1決定2）、queueのDBに書けない。2026-10-01に人は、observerの書き込みの経路をgoal 82のqueue serviceの形に揃え、jobのdagqをクライアントモードにしてservice経由で書くと決めた。goal 82のADRは、queue serviceをDBを開く唯一のプロセスにしてユースケース単位のAPIとservice側の認可を持たせ、dagqのクライアントモードを置き（[ADR-t1233-1](2026-10-02-t1233-1-control-and-execution-sides-queue-service-broker-and-client-mode.md)決定2〜4・7）、実行側のprincipalを制御側がjobごとに発行するtokenで認証し（[ADR-t1233-4](2026-10-02-t1233-4-queue-service-lifecycle-outage-notice-and-principal-tokens.md)決定4）、Codexのsandboxの中からserviceに届くようにし、observerなどqueueに書くjobをCodexに乗せるときも同じservice経由の経路で書くとした（[ADR-t1233-5](2026-10-02-t1233-5-read-use-cases-read-scope-by-role-and-codex-sandbox-reach.md)決定4・5。ADR-t813-3決定3をamends）。このADRは経路を選び直さず、observerに当てはめたときに決めることを決める。

## Decision

1. **Codexのobserverはqueue service経由で書く。** Codexのobserverのjobのdagqはクライアントモードで動き、findingの記録・更新・resolveとfindingに紐づく`blocked`のaskを、serviceのユースケースとして送る。jobのsandboxはread-onlyのままで、queue dirとDBのファイルへの書き込みもnetworkも開けず、ADR-t1233-5決定4のとおりserviceへの到達（socket、それを許せなければloopbackの宛先）だけを足す。sandboxを外すflagは使わない。promptが打たせるコマンドと出力の形は変えない。
2. **書ける範囲はservice側でobserverのroleに限る。** serviceはjobのtoken（ADR-t1233-4決定4。信頼する制御側のsupervisorがobserverのjobの起動ごとに発行し、jobの終わりで失効させる）からprincipalをobserverとそのactor idに決め、ADR-t728-1のpolicyで、読み取りと、findingの記録・更新・resolveとfindingに紐づく`blocked`のaskだけを許す。`finding dismiss`・note・mark・goalやtaskの変更・状態を変える他のコマンドは今と同じく拒み、拒否を記録する。クライアントが名乗るroleは認可に使わない。
3. **Claudeのobserverも同じ経路にする（providerに透過）。** ADR-t1063-1決定2の「jobはproviderを知らない」を保つため、observerのjobのdagqはproviderによらずクライアントモードで動かし、Claudeのobserverもservice経由で書く。これはADR-t1233-1決定8の段(3)（workerとjobのdagqをクライアントモードにしてDBのpathを渡さない）の一部で、Codexのobserverはクライアントモードが入ったときに初めて乗せられる。クライアントモードがjobに入る前にCodexのobserverを設定しても書けないので、それまでobserverのproviderにCodexを設定しない（goal 80の本番の切り替えは実装の着地の後）。
4. **記録の帰属と、今の書き込みで保つものはservice側で保つ。** service経由の書き込みでも、`finding_recorded` / `finding_updated` / `finding_status_changed`のpayloadの`by`と`ask_opened`の`asked_by`は今と同じくobserverとして残り、actorにはjobのactor idを記録する。同じ種類・対象・subjectの既存のfindingへの合流、新しいものの無い記録は何も書かないこと、根拠のevent idの検査、findingに紐づくaskの検査は、serviceのユースケースの中の1つのtransactionで今と同じ規則で行う（ADR-t1233-1決定3）。そのため`observe_finished`の件数とid、observer自身のeventを数えない判定（observer.mdの0）は、providerにもクライアントモードにも依らず今のまま成り立つ。
5. **Claudeの許す道具の代わりはread-onlyのsandboxとservice側の認可が果たし、Codexにhookやrulesは作らない（決定3のobserverの部分）。** Claudeのobserverのdagqだけを許す道具は、dagq以外のコマンドで書くこととsignalを防ぐためのもので、Codexではread-onlyのsandboxが書き込みと他のprocessへのsignalを拒み、queueへの書き込みはservice側の認可だけが通す。Claudeのobserverにはhookと拒むコマンドの一覧の設定が無いので、代わりを作るものも無い。Codexのobserverがdagq以外の読み取りのコマンドを打てる（ADR-t1063-1のspike 5.）ことは、goal reviewと同じく受け入れる。代わりを作らなかったことをdesignに残す。

ADR-t1063-1の他の決定（1・4〜7）と、決定2・3のobserver以外の部分は変えない。決定7の順のうちobserverの前提（「Codexのsandboxからqueueへ書く経路」）は、この決定1のservice経由の経路で満たす。

tokenの渡し方、serviceのユースケースの名前と欄、sandboxに足す設定、eventの欄名は実装のtask（task 1223、goal 82のtask 1235・1236）が[Observer](../design/supervisor-lifecycle/observer.md)と`docs/design/`に書く。

## Alternatives

- **(a) agentは書かず、最終の返答のデータでfindingとaskを返し、`dagq observe`のcommandが記録する**: read-onlyのsandboxのままで書け、他のjobのverdictと揃う。しかし、observerは既存のfindingを`findings`で読み、記録した結果（合流した・何も書かなかった）を見て次を決めながら書くので、1回の返答に落とすと観察の流れとpromptを作り直すことになる。また書き込みの経路がproviderの外に1つ増え、goal 82で全てのjobとworkerの書き込みをserviceに揃える形と割れる。人はqueue serviceの形に揃えると決めた（2026-10-01）。
- **(b) task 890のCodexのworkerの`dagq ask`と同じく、jobのdirに要求を書いてsupervisorが取り込む**: read-onlyのsandboxにjobのdirへの書き込みを足す必要があり、操作ごとに取り込みの形を足し、記録がsupervisorのpassまで遅れ、合流や拒否の結果をagentが見られない。ADR-t1233-5決定5は、この経路をクライアントモードが着地するまでの間だけのものにした。
- **Codexのobserverだけをクライアントモードにし、Claudeのobserverは今のままDBに直接書く**: jobのコードがproviderで分かれ、ADR-t1063-1決定2の透過に合わない。段(3)でjobからDBのpathを外す目的とも合わない。
- **Codexのobserverのsandboxをworkspace-writeにしてqueue dirを書けるようにする**: queue dirを書けることは他のrunの記録とworktreeを書けることを意味し、ADR-t813-3決定3がworkerについて退けたのと同じ理由で受け入れない。
- **Claudeのdagqだけを許す道具の代わりにCodexのrulesでdagq以外のコマンドを禁じる**: rulesは`sh -c`で回避でき（ADR-t813-3決定5）、read-onlyのsandboxとservice側の認可が防ぐもの以外に防ぐものが無い。

## Consequences

- Claudeのobserverの振る舞いは、書き込みがDBへの直接の書き込みからserviceのユースケースに替わるだけで、打つコマンド・出力・findingとaskの記録・`observe_finished`は変わらない。serviceが居ないとき、observerの書き込みはクライアントのerrorになり、DBを直接開くことへ戻らない（ADR-t1233-1決定7）。supervisorはserviceが居ない間jobの起動を控える（ADR-t1233-4決定2）。走っている間にserviceが落ちて書けなかったfindingは、そのobservationが失敗として終われば次のobservationが同じwindowを読み直すが、agentが正常に終われば読み直されない。書き込みのerrorをobservationの失敗に数えるかは実装のtask（task 1223）が決めてdesignに書く。
- observerのCodex化はgoal 82のクライアントモード（task 1236）とfindingのAPI（task 1235）の着地を待つ。plan review・スループットの見直し・復旧のCodex化はこれを待たない。
- host構成ではtokenもserviceの認可も助言的のまま（ADR-t1233-4決定5、ADR-t728-1決定6）。Codexのobserverではread-onlyのsandboxがDBのファイルへの直接の書き込みをOSで止めるので、Claudeのobserverより書き込みの経路が狭い。
