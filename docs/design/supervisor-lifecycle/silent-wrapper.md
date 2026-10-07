---
id: design-supervisor-lifecycle-silent-wrapper
type: design
title: "wrapperが黙ったsession"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1404-1
  - design-supervisor-lifecycle
---

# wrapperが黙ったsession

task 170。wrapperのheartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古くなっても、wrapperのプロセス（`run_processes`の`pid`）が生きていればsessionは生きている。以前はこれを`wrapper heartbeat expired; session may still be alive`のruntime errorにして手放し（resumeでは`resume_finished`の`outcome: error`）、supervisorはsessionの終了を待つだけだったので、sessionが何時間も残って着地が遅れた（task 107のresume 1回目: heartbeat切れから約2時間20分）。

- **判定**（`wrapper_pulse`）: worker と resume の監視は heartbeat が30秒より古いとき、wrapper の生存と終了の記録を確認する。終了を記録済みなら次の poll で終了を扱い、死んでいて終了の記録も無ければ失った session の error として復旧する。生きていれば `wrapper_heartbeat_expired`（`code: heartbeat_lost`、`pid`、`heartbeat_age_secs`、`workspace_id`）を沈黙ごとに1回記録する。heartbeat が戻れば記録済みの印を消す。
- **終了依頼**: 生きているが黙った wrapper には、`exit_requested` を記録して終了依頼の file を書く。worker の `/exit` の打鍵・画面の確認・時間切れの stuck_exit の ask は撤去した。過去の ask と event は履歴として読める。verdict の後の `ExitWatch` は終了依頼を1回書き、wrapper の終了を待つ。
- **wrapperの側**: heartbeatの書き込みが失敗しても、wrapperは子を待ち続け、理由をqueueのlog（`logs/session-*.jsonl`）に`wrapper heartbeat failed`として書く。終了の記録（`wrapper_exited`→`session_exited`）はSQLITE_BUSYなどの一時的な失敗で失わないよう、`application::session`の`record_exit`が上限付きで再試行する（`EXIT_RECORD_ATTEMPTS`は3回、間の待ちは200ミリ秒から倍々。applicationの層は一時的な失敗を見分けないのでerrorはすべて再試行する。1回ごとにbusy timeoutの5秒まで待ちうるので合計は最大約15.6秒で、supervisorがwrapperを黙ったとみなす30秒に収める。task 243）。失敗のたびに`recording wrapper exit failed, retrying`を、使い切ったら`wrapper exit not recorded after N attempts`を理由つきでlogに書き、wrapperはそのerrorで終わる（その後は上の「記録できないままプロセスが死んでいれば」のとおり）。
- supervisorはsessionをkillしない。heartbeatが止まった原因（DB書き込みの失敗など）はここでは扱わない。

**backgroundのwrapper**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）で`background`を選んだ非対話のsessionでは、wrapperが生きているかを`pid`だけでなく記録した起動時刻と合わせて判定する（`Supervisor::wrapper_lives`がそのpidの`wrapper_launched`の起動時刻と比べる。pidの再利用で別のprocessを生きているwrapperと見誤らず、死んだwrapperを黙ったwrapperとしない）。heartbeatの古さで黙ったとみなす判定と`wrapper_heartbeat_expired`、file による終了の依頼は今の非対話のrunと同じで、黙っただけではsupervisorはwrapperをkillしない（上の最後の項）。signalで止めるのは、cancel・`stop`・`stop_processes`・後始末など止める経路でhandleをcloseしたときだけ。詳細は[非対話のworker](headless-worker.md#workspaceなしのbackgroundのwrapper)。
