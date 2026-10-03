---
id: adr-t1394-2
type: adr
title: runtimeのplannerの経路（対話・非対話）をdagq.tomlの[roles.runtime_planner]で選べるようにし、非対話のplannerはworkerの非対話のturnの仕組みで動かし、revise・answer・促し・終了をturnの依頼として届け、区間とtokenをturnから記録する（ADR-t1228-1決定2〜5をamends）
status: accepted
created: 2026-10-03
updated: 2026-10-03
accepted_on: 2026-10-03
amended_by:
  - adr-t1433-2
  - adr-t1433-3
  - adr-t1533-1
amends:
  - adr-t1228-1 decision 2
  - adr-t1228-1 decision 3
  - adr-t1228-1 decision 4
  - adr-t1228-1 decision 5
owners:
  - hisamekms
tags:
  - runtime
  - planner
  - headless
related:
  - adr-t1394-1
  - adr-t813-1
  - adr-t813-2
  - adr-t1340-1
  - adr-t1404-1
  - adr-t1228-1
  - adr-0047
  - adr-0048
  - adr-0051
  - adr-t1091-1
  - adr-t598-1
  - design-supervisor-lifecycle-plan-planners
  - design-supervisor-lifecycle-headless-worker
  - design-supervisor-lifecycle-actor-model
---

# ADR-t1394-2: runtimeのplannerの経路（対話・非対話）を選べるようにし、非対話のplannerをworkerのturnの仕組みで動かす

## Context

runtimeのplannerはClaudeの対話のsession（cmuxのterminal、Stop hookのidle marker、`submit_input`での打ち込み、画面からのidleの推定）でだけ動く。workerは[ADR-t813-1](2026-09-28-t813-1-headless-worker-path.md)と[ADR-t1340-1](2026-10-02-t1340-1-claude-worker-defaults-to-headless.md)で非対話（turnごとの呼び出し）に移り、作業時間とtokenが減った。2026-10-02に人は、runtimeのplannerもworkerと同じく対話・非対話を切り替えられるようにし、非対話で1週間測って問題なければ続け、その後にCodexの非対話のplannerを別のgoalで作ると決めた（goal 87）。[ADR-t1394-1](2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)で人が開くplannerは無くなるので、plannerは全てruntimeが立てる。`--no-claude`では今runtimeのplannerが全く立たず、非対話化はCodexのplannerへの土台でもある。

## Decision

1. **選ぶ場所（(a)）。** 経路はmain checkoutの`dagq.toml`の`[roles.runtime_planner]`に経路の欄を足して選ぶ。runtimeの既定は評価が済むまで対話のままにし、まずこの repositoryの`dagq.toml`だけを非対話にして印を打ち、1週間後の評価で既定を変えるかを別のADRで決める。設定はplannerを立てる時点で読み、動いているplannerの経路は変えない。plannerごとに経路を選ぶ口（taskの`--interactive`に当たるもの）は作らない。人が画面を見る経路は人のplannerの廃止で無くなり、対話は評価のための比較と戻し先として残す。
2. **非対話のplannerはworkerのturnの仕組みで動かす（(b)）。** 非対話のplannerは、workerの非対話の経路（ADR-t813-1。依頼のファイル・turnの始まりと終わりの記録・idleの印・同じsessionを続けるturn）をplannerのディレクトリで使い、別の仕組みを作らない。初期promptが最初のturnで、reviseの指摘・`planner_question`のanswer・促しは打ち込みでなく次のturnの依頼として、終わらせるのは`/exit`でなく終了の依頼として送る。wrapperの置き場所（workspaceの中か、workspaceなしの切り離したprocessか）は[ADR-t1404-1](2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)のとおり同じ設定で選び、同決定8のとおり非対話のworkerと同じ形で動かす。
3. **終わりと生死（(c)）。** turnが終わって取られていない依頼が無いときをplannerのidleとし、画面からのidleの推定もStop hookも使わない。生きているかはwrapperの生死（workspaceの中ならworkspaceとwrapper、backgroundならADR-t1404-1決定2の識別）で決める。仕事を終えたplanner（submit・cancel・`keep_draft`・`declined`・findingの片付け）をidleで終わらせること、`planner_question`のanswerを待つあいだは終わらせないこと、決めずに終わったplannerを3回まで立て直すことは対話と同じ規則で、idleの材料だけをturnに置き換える。応答しないplannerのinboxへの知らせは、turnが動いているのにstallの閾値の上限を超えたときと、idleでも仕事が終わらないまま期限を過ぎたときに当てる（turnの上限はworkerと同じくwrapperがturnを止めて記録する）。答えを待つあいだは数えない。
4. **区間とtokenの記録（(d)）。** 非対話のplannerの区間とtokenは、pluginのhookでなくturnの記録から取る（非対話のworkerの区間と同じ、[ADR-0048](0048-record-claude-sessions-by-kind-with-open-and-active-time.md)のruntimeのplannerの区間）。区間は経路を持ち、`stats`のsessionのkindごとの集計と`kpi`のsessionの指標を経路で分けて読めるようにする。評価は同じ集計で対話の期間と比べる。
5. **使えないとき（(e)）。** 非対話のplannerはClaudeだけで動くので、providerの切り替えは無い。起動できない・認証・利用上限のときは、workerと同じく失敗にせずqueueの控え（Claudeのログイン切れと利用上限はqueueの控えのask）で待ち、控えが解けたら同じ依頼を続ける。この待ちは決めずに終わった回数（3回）に数えない。`--no-claude`では、Codexのplannerができるまで今までどおり立てない。
6. **Codexのplannerへの前提（(f)）。** Codexの非対話のplannerは別のgoalで作る。前提は、plannerのturnの駆動がproviderに透過であること（依頼・記録・usageの読み取り・区間がworkerのCodexのturnと同じ形で、planner固有の分岐をproviderに置かない）と、plannerの`dagq`がqueue serviceのクライアントとして動きCodexのsandboxからも打てること。この2つを満たすように非対話のplannerを作る。
7. **ADR-t1228-1のCLIを非対話のplannerに使う（(g)、ADR-t1228-1決定2〜5をamends）。**
   - **決定2（plannerへの依頼）**: 宛先を「生きている対話のplanner」から「生きているruntimeのplanner（対話・非対話）」に広げる。依頼のファイルをplannerのディレクトリに写し、写した先を指す決まった文を、非対話のplannerにはterminalへの送信でなく次のturnの依頼として置く。作業中のturnがあっても拒まず、依頼の順に、今のturnが終わってから届く（取られていない依頼をturnの順に取る仕組みのまま）。閉じた・`lost`・終了の依頼を送ったplannerへの依頼は拒む。
   - **決定3（plannerを閉じる）**: 非対話のplannerには終了の依頼を置き、wrapperが終わったことを確かめ、終わらなければADR-t1404-1決定3の手順で止めてから行を閉じる。閉じたことは対話と同じplannerの閉じた記録に残す。
   - **決定4（画面を読む）**: 非対話のplannerは画面を持たないので、非対話のrunと同じく画面が無いことを返し、plannerのディレクトリのturnの出力を指す。
   - **決定5（sessionに送る）**: 非対話のplannerには送れない。`planner_question`の答えは今までどおり`answer`でsupervisorが次のturnとして届け、終わらせるのは決定3の閉じる操作で行う。

経路の欄名と値、依頼のファイルとeventのkind・欄、時間切れの数値、区間の欄、CLIの返す値は[`plan` / `planners`](../design/supervisor-lifecycle/plan-planners.md)と[Actor model](../design/supervisor-lifecycle/actor-model.md)に書く。

## Alternatives

- **非対話を最初から既定にする**: 対話の基準値と比べる期間が無く、悪化しても戻す根拠が残らない。workerと同じく1つのrepositoryで切り替えて測る。
- **plannerごとに経路を選ぶ**: plannerはruntimeが立てるもので、人がtaskの単位で選ぶ理由が無い。評価にも経路が混ざるだけになる。
- **非対話のplannerを`claude -p`の1回の呼び出し（headlessのjob）にする**: reviseやanswerで同じsessionを続けられず、planner_questionの往復で文脈を失う。workerのturnは同じsessionを続ける形が既にある。
- **作業中のturnがあるときplannerへの依頼を拒む**: inboxが空くまで待って打ち直すことになる。依頼はturnの順に取られるので、置いておけば今のturnの後に届く。
- **非対話のplannerにも画面を作る**: turnの出力はplannerのディレクトリとlogで読め、送る操作は答えをaskに残す経路だけで足りる。

## Consequences

- runtimeのplannerは対話と非対話のどちらでも同じ結末（submit・cancel・`keep_draft`・`declined`・`planner_question`）に至り、`stats`と`kpi`で経路ごとに比べられる。
- 非対話のplannerには画面からのidleの推定・入力欄の送り直し・既知のダイアログの扱いが要らなくなる。
- inboxがplannerの様子を見るのは、非対話ではturnの記録とlogになる。
- `dagq.toml`に経路の欄を書くのは、固定バイナリがその欄を知ってから（知らない欄のある`dagq.toml`は古いバイナリが拒み、queue全体が止まる）。
- 評価の基準値・評価のコマンド・対話に戻す基準は、切り替えの印を打つtaskが文書にする。
