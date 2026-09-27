---
id: design-supervisor-lifecycle-abandon
type: design
title: "1 runの異常（abandon）"
status: current
created: 2026-09-26
updated: 2026-09-27
last_verified: 2026-09-27
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0025
---

# 1 runの異常（abandon）

wrapperのプロセスも死んでいるwrapper heartbeat切れ（プロセスが生きていれば手放さない。[wrapperが黙ったsession](silent-wrapper.md#wrapperが黙ったsession)）、検証処理そのもの（Git呼び出しやDB）の失敗、closeの記録失敗など、監視中のruntime errorは**そのrunだけ**を手放す: `last_error`と`runtime_error`イベント（`lease_released: true`）を書き、そのrunのlease行を削除し、status・`run_processes`・workspace・worktreeは変えない。同じtransactionで、そのrunのcloseされていない`stalled`のaskを閉じ、結末の無い受領の無いidleの検知に`stall_resolved`（`outcome: run_ended`）を1回ずつ記録する（[receiptの無いidleの検知](idle-without-receipt.md)の「閉じる」、task 379）。supervisorは他のrunを続け、結果の`errors`にそのrunを載せる。leaseを消すのは、常駐supervisorが生きている間も`recover`がrunのprocessだけで判定できるようにするため。taskは未完了runで占有されたままなので二重実行にはならず、未登録のwrapperはleaseがなければ登録できずClaudeを起動しない。leaseがないので他のsupervisorも引き継がない。`status` / `watch`はこのrunを`recover run`のattentionとして出す（[ADR-0025](../../adr/0025-leaseless-unfinished-run-is-a-recover-run-attention.md)。`kind`は`runtime_error`）。sessionのprocessが残っていなければ（未登録、`exited_at`が記録済み、PIDが死んでいる）、次のfill passでsupervisor自身がrecoverして`interrupted`にし、triageに回す（ADR-0044の決定3。[Triage (supervisor)](triage.md#triage-supervisor)）。processが生きている間は`recover run`のまま人を待つ。人は`dagq-recover` skillに従い、`show`と`doctor`で確認して[recover](recover.md#recover-run_id)で扱う（supervisorが居ないときだけ。居ればprocessが止まった時点でsupervisorが行う）。recoverすればrunは`interrupted`（`integrating`だったものは`awaiting_integration`）になりattentionから消える。wrapperの登録を待つ時間は`WorkspaceBackend::registration_timeout`（既定45秒）。終了要求のtimeout（`exit_request_timed_out`）ではabandonせず、leaseを持ったままsessionの終了を待つ（[Receipt and session exit](receipt-and-session-exit.md#receipt-and-session-exit)）。

validating・review・revise・verdictの後の`/exit`の途中（[Review](review.md#review-supervisor)でsupervisorがworkerのsessionを開いたまま持っている間）にruntime errorで手放すときは、手放す前にそのsessionへ`/exit`を送る（task 237）。手放したrunはleaseが無く、supervisorもadoptも見ないので、送らなければsessionは誰も見ないまま開いて残る。送るのは他の`/exit`と同じ`submit`で、sessionが既に終わっている（wrapperが終了を記録したか死んでいる）、workspaceが既に無い、leaseが自分のものと確かめられない（別のsupervisorが引き継いだかもしれない）ときは送らない。`/exit`を要求済み（verdictの後の`/exit`の待ちで、届かなかった`exit_unsent`ではないもの）のsessionには二度送らない（ダイアログの選択肢を選びうるため）。結果は`runtime_error`のpayloadの`session`（`workspace_id`、`exit`: `sent` / `requested_before` / `failed`、`failed`なら`error`）に残す。`failed`（cmuxに送れない、ダイアログが出ている、入力欄に残った、届かなかった）なら、そのrunのattentionは`exit the session`（`domain::AttentionNext::ExitSession`）になり、人がworkspaceで`/exit`を打つ。`session_exited`が記録されると、runは元のattention（`awaiting_integration`なら`review and integrate`、leaseの無い未完了runなら`recover run`）に戻る。`events` / `watch`もその`runtime_error`を`exit the session`として返す。validatingとreviewの工程は、errorで手放すときにsessionが分かるよう、次の工程に移るまでsessionを工程に残しておく。`exit the session`に置き換えるのは`review and integrate`と`recover run`だけで、`needs_session`・`failed`のrun（検証の失敗やreviseの打ち切りの後の`/exit`の途中で手放したもの）は`resuming (runtime)`・`triaging (runtime)`のまま出る（resumeは生きているsessionの終了を先に待つ）。
