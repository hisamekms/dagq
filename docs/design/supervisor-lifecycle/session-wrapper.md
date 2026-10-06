---
id: design-supervisor-lifecycle-session-wrapper
type: design
title: "`session` wrapper"
status: current
created: 2026-09-26
updated: 2026-10-06 # task 1438: the wrapper runs only headless turns; task 1441: the runtime's planner wrappers start only in the background too; task 1440: the runtime starts a run's wrapper only with --background; task 1406
last_verified: 2026-10-06 # task 1438; task 1441; task 1440
scope: runtime
related:
  - adr-t1433-3
  - adr-t1433-2
  - adr-t1404-1
  - design-supervisor-lifecycle
---

# `session` wrapper

> **goal 92**: [ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)で対話の経路を廃止し、[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)（task 1440）でruntimeはrunのsession wrapperを、cmux workspaceなしで、supervisorから切り離したbackgroundのprocess（下の「backgroundのwrapper」）としてだけ起動する。`dagq.toml`の`[headless] wrapper`はworkerには効かない（[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)の「設定」）。runtimeのplannerのwrapper（`planner-session`）もtask 1441から`--headless --background`でだけ起動し、workspaceを開かない（[Plan planners](plan-planners.md)）。

runtimeが`--background`を付けて起動する隠しコマンド（`session`）。ユースケースは`src/application/session.rs`の`run_session`で、queue（`Queue`）、agentのコマンド（`AgentProvider`）、その起動（`Spawner`）、`prompt.txt`の読み取り（`RunFiles`）とwrapperのpidを`Session`として受け取る。`runtime::session`は`SqliteQueue`、`ClaudeCode`、`LocalSpawner`、`LocalRunFiles`を渡す入口だけ。`--background`でない入口（TTYを確かめ、パイプからは起動しない。cmux workspaceの中でworkspaceの記録を待つ）はコードに残るが、runtimeはworkerのwrapperもplannerのwrapperもそれで起動しない。通るのは`dagq session`を`--background`なしで打ったとき（端末が無ければ拒む）だけ。workerのwrapperをそれで動かすtestとcomposeの入口は消した（入口そのものを消すのは後続のtask）。

1. supervisorが記録した自分の起動（runの最後の`wrapper_launched`の`pid`と`start`が自分のもの）を待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、`src/application/headless_session.rs`の`Turns`が1 turnごとにproviderの非対話の呼び出しを起動する: run dirの`turns/`の依頼を待ってsessionをresumeし、turnを記録してidle markerを書き、終了の依頼で終わる（[非対話のworker](headless-worker.md)）。最初のturnのprocessを`agent`として登録し（`agent_started`、runが`running`になる）、後のturnは起動のたびに同じ`agent`の行の`pid`をそのturnのprocessに差し替える（`register_turn_agent`。eventも状態遷移も無い。task 862）。turnの環境は、workerのactorの変数に、queue serviceのsocket（`DAGQ_SERVICE_SOCKET`）とsupervisorがclaimかresumeで発行したworkerのtokenのfile（`DAGQ_SERVICE_CREDENTIAL_FILE`。無ければwrapperが発行する）を足し、`DAGQ_QUEUE`を外したもので、agentの`dagq`はクライアントモードで動く（goal 82の段(3)、[Queue service](../queue-service.md#クライアントモード)）。wrapper自身は`--db`でqueueを開き、wrapperのprocessのenvにはどちらも無い。
3. 1秒ごとにheartbeatを更新しながらturnのプロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。turnの起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

対話のagentを1つ起動してその終わりをwaitする経路（`worker_mode`が`interactive`のrun）は、対話のworkerの廃止（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）で無い。`interactive`と記録されたrunもclaimとresumeで非対話に変わるので（task 1437）、wrapperは常に`Turns`で動く。

**backgroundのwrapper**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）と[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)。supervisorはrunのwrapperを、cmuxのworkspaceでなく切り離したprocess（`setsid`、親は1）として`session ... --background`で起動する（最初のsession・resume・開き直しのどれも。`background::wrapper_command`は必ず`--background`を付ける）。入口（`compose::wrapper_entry`）はTTYを確かめず、`CMUX_WORKSPACE_ID`の自分のworkspaceを持たずに自分で`setsid`する。1のとおり、supervisorが記録した起動（`wrapper_launched`の`pid`が自分のpid）を待って登録する（`WrapperStart`）。envはprocessのenvで受け、`[dagq]`の要約は端末が無くてもrun dirの`session.log`に書き（`Turns`の`background`、task 1406）、人は`dagq run log RUN [--follow]`で読む。止めるのは終了の依頼と、pid・process groupへのsignal（SIGTERMの後に残ればSIGKILL）。詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)。
