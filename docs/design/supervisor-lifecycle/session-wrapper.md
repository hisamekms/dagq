---
id: design-supervisor-lifecycle-session-wrapper
type: design
title: "`session` wrapper"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1433-3
  - adr-t1433-2
  - adr-t1404-1
  - design-supervisor-lifecycle
---

# `session` wrapper

> runtimeはrunのsession wrapperを、cmux workspaceなしで、supervisorから切り離したbackgroundのprocess（下の「backgroundのwrapper」）としてだけ起動する（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
> `dagq.toml`の`[headless] wrapper`はworkerには効かない（[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)の「設定」）。
> runtimeのplannerのwrapper（`planner-session`）もbackgroundでだけ起動し、workspaceを開かない（[Plan planners](plan-planners.md)）。

runtimeがbackgroundで起動する隠しコマンド（`session`）。
ユースケースは`src/application/session.rs`の`run_session`。
backgroundでない入口（TTYを確かめ、パイプからは起動しない。cmux workspaceの中でworkspaceの記録を待つ）は古いsupervisorのargvのために受け付けるが、runtimeはworkerのwrapperもplannerのwrapperもそれで起動しない。
通るのは`dagq session`をbackgroundの指定なしで打ったとき（端末が無ければ拒む）だけ。
どちらの入口でも、登録を断られたwrapperは断られたことをlogに書いてerrorで終わり、動いているworkspaceを閉じない（cmuxを呼ばない）。

1. supervisorが記録した自分の起動（runの最後の`wrapper_launched`が自分のもの）を待ち、wrapperのPIDを一度だけ登録する。
   leaseが無効なら登録できない。
2. `prompt.txt`を読み、`src/application/headless_session.rs`の`Turns`が1 turnごとにproviderの非対話の呼び出しを起動する: run dirの`turns/`の依頼を待ってsessionをresumeし、turnを記録してidle markerを書き、終了の依頼で終わる（[非対話のworker](headless-worker.md)）。
   最初のturnのprocessを`agent`として登録し（`agent_started`、runが`running`になる）、後のturnは起動のたびに同じ`agent`の行の`pid`をそのturnのprocessに差し替える（eventも状態遷移も無い）。
   turnの環境は、workerのactorの変数に、queue serviceのsocketとsupervisorがclaimかresumeで発行したworkerのtokenのfile（無ければwrapperが発行する）を足したもので、agentの`dagq`はクライアントモードで動く（[Queue service](../queue-service.md#クライアントモード)）。
   wrapper自身はqueueを直接開き、wrapperのprocessのenvにはどちらも無い。
3. heartbeatを更新しながらturnのプロセスをwaitする。
   DB障害中も子プロセスの所有を手放さない。
4. 終了コードを`session_exited`として記録する。
   turnの起動後のエラーでは子プロセスが生きている可能性を考慮し、終了を記録しない。

`interactive`と記録されたrunもclaimとresumeで非対話に変わるので、wrapperは常に`Turns`で動く。

**backgroundのwrapper**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)と[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)。
supervisorはrunのwrapperを、cmuxのworkspaceでなく切り離したprocess（`setsid`、親は1）として起動する（最初のsession・resume・開き直しのどれも）。
入口（`compose::wrapper_entry`）はTTYを確かめず、自分で`setsid`する。
1のとおり、supervisorが記録した起動（`wrapper_launched`が自分のもの）を待って登録する。
envはprocessのenvで受け、`[dagq]`の要約は端末が無くてもrun dirの`session.log`に書き、人は`dagq run log`で読む。
止めるのは終了の依頼と、pid・process groupへのsignal（SIGTERMの後に残ればSIGKILL）。
詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)。
