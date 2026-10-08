---
id: design-supervisor-lifecycle-backend-call-failures
type: design
title: "backendの呼び出しの失敗"
status: current
created: 2026-09-26
scope: runtime
related:
  - design-supervisor-lifecycle
  - adr-0013
  - adr-0012
  - design-domain-model
---

# backendの呼び出しの失敗

`WorkspaceBackend`（cmux adapter）か`SessionWrappers`（backgroundのwrapper）の呼び出しが失敗するかtimeoutすると（どの呼び出しも30秒、各portの`call_timeout`）、runtimeは`backend_call_failed`をrun_eventsに記録する（task 109）。負荷が高いとcmuxが詰まることをqueueに残し、observerが`stats`の`backend_failures`から並列度の見直しを根拠つきで提案できるようにするためで、記録するのはruntime、observerは`stats`を読むだけ。

- **payload**: `code`（`backend_timeout` / `backend_failed`。[domain-model](../domain-model.md#理由の分類コードcode)）、`op`（`launch_background` / `create_named` / `close` / `exists` / `ensure_group`など。過去の記録にはrunのworkspaceを作った`create` / `create_resume`と、対話のsessionの画面と打鍵の`capture` / `send_text` / `send_enter` / `send_key` / `send_exit`もある。askの通知はinboxの`watch`が記録の無いcmux adapterで出すので、opに無い（[人への通知](cmux-notify.md#人への通知cmux-notify)）。`up`の`preflight`はcmuxに繋がるかの確認で、失敗すればコマンド自体が止まるので記録しない）、`workspace_id`（無い呼び出しはnull）、`timeout_secs`、`error`（先頭300文字）、`load_avg`（getloadavg(3)の1分値。取れなければnull）、`slots`、`parallel`、`attempt`（何回目の呼び出しか。1から）、`max_attempts`（その呼び出しに許す回数。retryしない呼び出しは1）、`retry_after_ms`（次の試行までのbackoff。retryしなければnull）。
- **timeoutのretry**（task 326）: timeoutした呼び出しは、もう一度呼んでも害が無いものだけ、backoffを置いて各portの`call_attempts`（既定3）回までretryする。
  backoffは各portの`retry_backoff`（既定2秒）から始めて毎回倍にする。
  失敗した試行はretryするものも含めて1回ずつ`backend_call_failed`になるので、`backend_failures`の件数はretryの分も数える（`stats`はそのうちretryした試行を`retried`、使い切った失敗を`exhausted`に分けて出す。task 397）。
  上限まで失敗したときだけ呼び出し元に今までどおりのエラーを返す。
  retryするのは読むだけの`exists`（`WorkspaceBackend`と`SessionWrappers`の両方）で、ほかの呼び出しは1回だけ呼ぶ。
  `WorkspaceBackend`にはsessionの画面を読む・打ち込むmethodが無く、supervisorはどのsessionにも打ち込まない。
  - workerとplannerの終了は`turns/exit`の依頼で、supervisorはinboxにも打ち込まない。
  - workerのsubmitはturnの依頼を書き、画面を読まない。
  - supervisorはどのsessionにも打ち込まず、workerの起動の確認と復旧も画面ではなくturnの記録で判断する（[非対話のworker](headless-worker.md)）。
- **run**: runのための呼び出し（runのsession wrapperを起動する`launch_background`（envの`DAGQ_RUN_ID`のrun）、runの`task_runs.workspace_id`のhandleの停止（`stop_background`。失敗はop `close`）はそのrunのイベントとして`task_id`と`run_id`を持つ。runからは引けない呼び出し（`up`のinbox workspaceの`create_named`と`exists`、`up`が求めるqueueのworkspace groupの`ensure_group`、`up`のdrainと`down`が閉じる登録済みのin-cmuxのsupervisorのworkspaceの`close`）は`task_id`も`run_id`も持たない（0012で`run_events`のCHECKがこのkindだけに認める）。plannerのwrapperの`launch_background`はenvがrunを名指さないので記録せず、失敗をそのまま返す（`RecordingSessions::launch_background`）。supervisorはrunのためにworkspaceもgroupも作らない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、task 1440）。runtimeのplannerも`ActorProgram::PlannerSession`で`launch_background`から起動し、workspaceもgroupも作らない（`create_named`も`ensure_group`も呼ばない。[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、task 1441）。
- **slots / parallel**: supervisorの呼び出しはそのsupervisorのtokenのlease数（握っているslot）と`--parallel`。`up` / `down`の呼び出しは全leaseの数と、登録済みsupervisorの`parallel`の合計（登録が無ければnull）。
- **記録する場所**: cmux adapterではなくapplication層の`application::recording`の`RecordingBackend`（`WorkspaceBackend`を包むdecorator）と`RecordingSessions`（`SessionWrappers`を包むdecorator。どちらも`runtime`から再公開し、DBのpathから作る`new`は`compose`にある。`up`・`down`は`QueueOpener`から`over`で作る）が、`up`・`down`と`supervise`で渡されたportを包んで記録する（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)。`supervise`ではユースケースが自分のtokenで包む）。記録は`QueueOpener`が開く自前の接続で書き、書けなくても呼び出し元へ返すエラーは元のまま。needs_sessionのresume（[`needs_session`](needs-session.md#needs_session)）のwrapperの`launch_background`（runに記録）と、そのhandleの停止（`stop_background`、失敗はop `close`）も同じ経路を通る。止めた結果は失敗と別に`wrapper_stopped`に記録する（[非対話のworker](headless-worker.md)の「停止の記録」）。resumeのhandleは`task_runs.workspace_id`に無いので、`close`の失敗はrunを持たない記録になる（`launch_background`はrunに付く）。
- 既存の記録はそのまま残す: closeの失敗は`cleanup_failed`で、同じ失敗を`backend_call_failed`としても記録する（呼び出しの直後なので`backend_call_failed`が先）。過去の記録の`screen_capture_failed`（wrapper終了後の`read-screen`の失敗）も同じ形で`backend_call_failed`を伴う。`/exit`後にsessionが終わらない`exit_request_timed_out`（task 1437より前の記録）はcmuxの呼び出しの失敗ではないので`backend_call_failed`にならない（同じ時期の記録で`backend_call_failed`になったのは`/exit`の送信（`send_exit`）そのものが失敗かtimeoutしたときだけ）。runのwrapperの`launch_background`の失敗（provisioningの失敗で、以後のclaimも止める。resumeの起動の失敗は`resume_finished`の`outcome: error`）はrunをabandonするが、`backend_call_failed`はabandonの`runtime_error`より前に入る。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。wrapperが生きている（heartbeatが30秒以内か`exited_at`記録済み）`running` / `validating`のrunだけは、staleなleaseごと次のsupervisorが引き継いで同じrunを続ける（[ADR-0012](../../adr/0012-adopt-stale-lease-of-live-wrapper.md)、[`supervise`](supervise.md#supervise)の5）。`awaiting_integration`のrunはwrapperを問わず引き継ぐ（task 236）。それ以外（wrapperが死んだ・黙った、`claimed` / `starting`、`integrating`、leaseなし）は、processが止まっていてleaseが無いかleaseのpidが死んでいればsupervisorが自分で`recover`してtriageにかけ、そうでなければユーザーが`recover`で明示的に復旧する。
