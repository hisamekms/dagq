---
id: design-supervisor-lifecycle-session-wrapper
type: design
title: "`session` wrapper"
status: current
created: 2026-09-26
updated: 2026-10-03
last_verified: 2026-10-03
scope: runtime
related:
  - adr-t1404-1
  - design-supervisor-lifecycle
---

# `session` wrapper

cmux workspaceが起動する隠しコマンド。TTYが必要で、パイプからは起動しない（`--background`で起動されたものを除く。下の「backgroundのwrapper」）。ユースケースは`src/application/session.rs`の`run_session`で、queue（`Queue`）、agentのコマンド（`AgentProvider`）、その起動（`Spawner`、標準入出力は端末を継承）、`prompt.txt`の読み取り（`RunFiles`）とwrapperのpidを`Session`として受け取る。`runtime::session`はTTYを確かめ、`SqliteQueue`、`ClaudeCode`、`LocalSpawner`、`LocalRunFiles`を渡す入口だけ。

1. `workspace_id`が保存されるまで待ち（45秒以内）、wrapperのPIDを一度だけ登録する。leaseが無効なら登録できない。
2. `prompt.txt`を読み、providerのコマンドでagentを起動して`agent_started`を記録し、runを`running`にする。agentの環境は、workerのactorの変数に、queue serviceのsocket（`DAGQ_SERVICE_SOCKET`）とsupervisorがclaimかresumeで発行したworkerのtokenのfile（`DAGQ_SERVICE_CREDENTIAL_FILE`。無ければwrapperが発行する）を足し、`DAGQ_QUEUE`を外したもので、agentの`dagq`はクライアントモードで動く（goal 82の段(3)、[Queue service](../queue-service.md#クライアントモード)）。wrapper自身は`--db`でqueueを開き、workspaceの環境にはどちらも無い。非対話のturnも同じ環境で起動する。
3. 1秒ごとにheartbeatを更新しながら子プロセスをwaitする。DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。agent起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

runの`worker_mode`が`headless`なら、2と3の代わりに`src/application/headless_session.rs`の`Turns`が1 turnごとに非対話の呼び出しを起動し、run dirの`turns/`の依頼を待ってsessionをresumeし、turnを記録してidle markerを書き、終了の依頼で終わる（[非対話のworker](headless-worker.md)）。最初のturnのprocessを`agent`として登録し（`agent_started`、runが`running`になる）、後のturnは起動のたびに同じ`agent`の行の`pid`をそのturnのprocessに差し替える（`register_turn_agent`。eventも状態遷移も無い。task 862）。

**backgroundのwrapper**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）で、`dagq.toml`の`[headless] wrapper = "background"`を選んだ非対話のsessionでは、supervisorがwrapperをcmuxのworkspaceでなく切り離したprocess（`setsid`、親は1）として`session ... --background`で起動する。入口（`compose::wrapper_entry`）はTTYを確かめず、`CMUX_WORKSPACE_ID`の自分のworkspaceを持たずに自分で`setsid`する。1の`workspace_id`の代わりに、supervisorが記録した起動（`wrapper_launched`の`pid`が自分のpid）を待って登録する（`WrapperStart`）。envはworkspaceの`--env`でなくprocessのenvで受け、`[dagq]`の要約はrun dirの`session.log`に書く。止めるのはworkspaceのcloseのhangupでなく、終了の依頼と、pid・process groupへのsignal（SIGTERMの後に残ればSIGKILL）。workspaceのときは今のまま。詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)。
