---
id: design-supervisor-lifecycle-silent-wrapper
type: design
title: "wrapperが黙ったsession"
status: current
created: 2026-09-26
updated: 2026-10-03
last_verified: 2026-09-27
scope: runtime
related:
  - adr-t1404-1
  - design-supervisor-lifecycle
---

# wrapperが黙ったsession

task 170。wrapperのheartbeatが`HEARTBEAT_TIMEOUT_SECS`（30秒）より古くなっても、wrapperのプロセス（`run_processes`の`pid`）が生きていればsessionは生きている。以前はこれを`wrapper heartbeat expired; session may still be alive`のruntime errorにして手放し（resumeでは`resume_finished`の`outcome: error`）、supervisorはsessionの終了を待つだけだったので、sessionが何時間も残って着地が遅れた（task 107のresume 1回目: heartbeat切れから約2時間20分）。

- **判定**（`wrapper_pulse`）: 監視中のsession（workerの`SessionWatch`、resumeの`ResumeWatch`、reviseの`ReviseWatch`、verdictの後の`ExitWatch`）はpollのたびにwrapperのheartbeatを見る。30秒以内なら従来どおり。古くてPIDが死んでいれば従来どおりのerror（同じ文言で、workerはabandon、resumeは`give_up_resume`）で、`/exit`は送らない。古くてPIDが生きていれば、`wrapper_heartbeat_expired`（`pid`、`heartbeat_age_secs`、`workspace_id`。沈黙ごとに1回（`/exit`の後の印と`ExitWatch`の印は消さないので、そこではwatchごとに1回）。run_eventsのkindの追加で、attentionではない）を記録してlogに書き、次へ進む。
- **heartbeatが戻ったとき**（task 606）: 黙った印（`SessionWatch.silent`・`ResumeWatch.silent`・`Revise`の`live.silent`）は、`/exit`を送る前（`exit_requested`が無い）に30秒以内のheartbeatを見たpollで消す（`Revise`は`observe`がheartbeatを新しいと見たとき）。待ちの見張りが一時的な沈黙で待ちを`wrapper_silent`で終え、slotに戻ったときにはheartbeatが戻っていたrunは、その段で再び待ちに入れ、後の本当の沈黙では`wrapper_heartbeat_expired`を改めて記録する。`/exit`を送った後の印と`ExitWatch.silent`は消さない（下の`stuck_exit`のaskの文言は`exit_for_silence`が持ち、PIDの死で`stuck_exit`のaskを閉じるのにこの印を使うため）。
- **`/exit`**: 通常の終了と同じ経路で1回だけ送る。workerのsessionはreceiptの有無に関わらず`exit_requested`を記録してから送り（receiptが無ければ終了後の検証が`failed`にし、triageに回る）、黙っている間はanswerの配送とダイアログ待ちの検知をしない。resumeしたsessionは解消依頼の前でも送る。reviseを待っていたsessionは`ReviseOutcome::Ended`（`went silent ...`）で`approve_landing`のaskへ進み、そのための`ExitWatch`が送る。`ExitWatch`は既に送っていれば送り直さない。
- **閉じないとき**: `exit_timeout`（cmuxは120秒）を過ぎれば、それぞれの既存の経路のまま`exit_request_timed_out`とinbox宛ての`stuck_exit`のaskになる（workerはleaseを持ったまま`running`で待ち、resumeは`unresolved`で手放し、`ExitWatch`はverdictの後のstatusで待つ）。`/exit`をheartbeat切れのために送ったときだけ（`exit_for_silence`）、askのquestionの「sessionが終わった後」の文の前にそのこと（`SILENT_WRAPPER_EXIT`）を置く。`/exit`を送った後で黙った場合は置かない。
- **askのkind**: 新しいkindを作らず`stuck_exit`を使う。人に求める判断（画面を読み、ダイアログがあれば応答して`/exit`を送るか、`wait`でそのままにするか）、options（`exit` / `wait`）、sessionが終わったらruntimeがaskを閉じること、drainがsessionの終了を待つことが、ダイアログで`/exit`が止まった場合と同じで、inboxと`dagq-recover` skillの`reference/stuck-exit.md`がそのまま扱えるため。heartbeat切れはquestionの文とrun_eventsの`wrapper_heartbeat_expired`で区別できる。
- **終わった後**: wrapperが`session_exited`を記録すれば通常どおり進む（workerは`supervision_finished`→`validating`、resumeは書き直したreceiptで判定、`ExitWatch`はworkspaceを閉じてverdictのとおり）。PIDの死を見たときはwrapperの行を読み直し、終了が記録されていれば（読んだ後に記録して死んだ）次のpollで通常の終了として扱う。記録できないままプロセスが死んでいれば従来どおりのerrorになり、黙ったことで開いた`stuck_exit`のaskは終わらせるsessionが無いのでそのときに閉じる（`the session exited; closed by the runtime`）。
- **wrapperの側**: heartbeatの書き込みが失敗しても、wrapperは子を待ち続け、理由をqueueのlog（`logs/session-*.jsonl`）に`wrapper heartbeat failed`として書く。終了の記録（`wrapper_exited`→`session_exited`）はSQLITE_BUSYなどの一時的な失敗で失わないよう、`application::session`の`record_exit`が上限付きで再試行する（`EXIT_RECORD_ATTEMPTS`は3回、間の待ちは200ミリ秒から倍々。applicationの層は一時的な失敗を見分けないのでerrorはすべて再試行する。1回ごとにbusy timeoutの5秒まで待ちうるので合計は最大約15.6秒で、supervisorがwrapperを黙ったとみなす30秒に収める。task 243）。失敗のたびに`recording wrapper exit failed, retrying`を、使い切ったら`wrapper exit not recorded after N attempts`を理由つきでlogに書き、wrapperはそのerrorで終わる（その後は上の「記録できないままプロセスが死んでいれば」のとおり）。
- supervisorはsessionをkillしない。heartbeatが止まった原因（DB書き込みの失敗など）はここでは扱わない。

**予定（未実装）**: [ADR-t1404-1](../../adr/2026-10-03-t1404-1-headless-wrappers-run-as-detached-background-processes.md)（goal 89）で`background`を選んだ非対話のsessionでは、wrapperが生きているかを`pid`だけでなく記録した起動時刻と合わせて判定する（pidの再利用で別のprocessを生きているwrapperと見誤らない）。heartbeatの古さで黙ったとみなす判定と`wrapper_heartbeat_expired`、`/exit`が終了の依頼になることは今の非対話のrunと同じで、黙っただけではsupervisorはwrapperをkillしない（上の最後の項）。signalで止めるのは、cancel・`stop`・`stop_processes`・後始末など止める経路に入ったときだけ。詳細は[非対話のworker](headless-worker.md#予定-workspaceなしのbackgroundのwrapper)。
