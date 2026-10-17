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

`SessionWrappers`（backgroundのwrapper）の呼び出しが失敗するかtimeoutすると（portの`call_timeout`）、runtimeは`backend_call_failed`をrun_eventsに記録する。
負荷が高いとsessionの起動や停止が詰まることをqueueに残し、observerが`stats`の`backend_failures`から並列度の見直しを根拠つきで提案できるようにするためで、記録するのはruntime、observerは`stats`を読むだけ。

- **payload**: 理由の分類の`code`は[domain-model](../domain-model.md#理由の分類コードcode)が持つ。
  欄と今の`op`の名前は`application::recording`のdoc commentが持つ。
  過去の記録には古いバイナリのcmuxの呼び出しのopもあり、`stats`と`events`はそれも読む（`domain::stats::BackendFailures`）。
- **timeoutのretry**: timeoutした呼び出しは、もう一度呼んでも害が無いものだけ、backoffを置いて各portの`call_attempts`回までretryする。
  backoffは各portの`retry_backoff`から始めて毎回倍にする。
  失敗した試行はretryするものも含めて1回ずつ`backend_call_failed`になるので、`backend_failures`の件数はretryの分も数える（retryした試行と使い切った失敗の内訳は`domain::stats::BackendFailures`）。
  上限まで失敗したときだけ呼び出し元にretryしないときと同じエラーを返す。
  retryするのは読むだけの`exists`で、ほかの呼び出しは1回だけ呼ぶ。
  `SessionWrappers`にはsessionの画面を読む・打ち込むmethodが無く、supervisorはどのsessionにも打ち込まない。
  - workerとplannerの終了は`turns/exit`の依頼で、supervisorはinboxにも打ち込まない。
  - workerのsubmitはturnの依頼を書き、画面を読まない。
  - supervisorはどのsessionにも打ち込まず、workerの起動の確認と復旧も画面ではなくturnの記録で判断する（[非対話のworker](headless-worker.md)）。
- **run**: runのための呼び出し（runのsession wrapperの起動（envの`DAGQ_RUN_ID`のrun）と、runのhandleの停止（失敗はop `close`））はそのrunのイベントとしてtaskとrunに紐づく。
  runからは引けない呼び出し（plannerとresumeのwrapperのhandleの停止の`close`と生死の`exists`、過去の記録の`up` / `down`のcmuxの呼び出し）は`task_id`も`run_id`も持たない（`run_events`のCHECKがこのkindだけに認める）。
  plannerのwrapperの起動はenvがrunを名指さないので記録せず、失敗をそのまま返す（`RecordingSessions::launch_background`）。
  supervisorはrunのためにworkspaceもgroupも作らない（[ADR-t1433-3](../../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
  runtimeのplannerも同じbackgroundの起動（`ActorProgram::PlannerSession`）で起動し、workspaceもgroupも作らない（[ADR-t1433-2](../../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
- **slots / parallel**: supervisorの呼び出しはそのsupervisorのtokenのlease数（握っているslot）と`--parallel`。
  tokenの無い呼び出し（過去の記録の`up` / `down`のcmuxの呼び出しだけ）は全leaseの数と、登録済みsupervisorの`parallel`の合計（登録が無ければnull）。
- **記録する場所**: adapterではなくapplication層の`application::recording::RecordingSessions`（session wrapperのportを包むdecorator。`runtime`から再公開し、DBのpathから作るものは`compose`にある）が、渡されたportを包んで記録する（[ADR-0013](../../adr/0013-layered-architecture-and-type-function-style.md)。`supervise`ではユースケースが自分のtokenで包む）。
  `RecordingSessions`は記録とwrapperが残したturnの読み取りに要る実行と着地のportだけを開くopenerを取り、`supervise`にはcomposeがそれを注入する。
  記録はそのopenerが開く自前の接続で書き、書けなくても呼び出し元へ返すエラーは元のまま。
  needs_sessionのresume（[`needs_session`](needs-session.md#needs_session)）のwrapperの起動（runに記録）と、そのhandleの停止（失敗はop `close`）も同じ経路を通る。
  止めた結果は失敗と別に`wrapper_stopped`に記録する（[非対話のworker](headless-worker.md)の「停止の記録」）。
  resumeのhandleは`task_runs.workspace_id`に無いので、`close`の失敗はrunを持たない記録になる（`launch_background`はrunに付く）。
- 既存の記録はそのまま残す: closeの失敗は`cleanup_failed`で、同じ失敗を`backend_call_failed`としても記録する（呼び出しの直後なので`backend_call_failed`が先）。
  過去の記録の`screen_capture_failed`（wrapper終了後の`read-screen`の失敗）も同じ形で`backend_call_failed`を伴う。
  `/exit`後にsessionが終わらない`exit_request_timed_out`（過去の記録）はbackendの呼び出しの失敗ではないので`backend_call_failed`にならない（同じ時期の記録で`backend_call_failed`になったのは`/exit`の送信そのものが失敗かtimeoutしたときだけ）。
  runのwrapperの起動の失敗（provisioningの失敗で、以後のclaimも止める。resumeの起動の失敗は`resume_finished`のerrorの終わり）はrunをabandonするが、`backend_call_failed`はabandonの`runtime_error`より前に入る。

supervisorの再起動ではrunごとのleaseとheartbeatを確認し、孤児プロセスを勝手に再実行しない。
wrapperが生きている（heartbeatが`HEARTBEAT_TIMEOUT_SECS`以内か`exited_at`記録済み）`running` / `validating`のrunだけは、staleなleaseごと次のsupervisorが引き継いで同じrunを続ける（[ADR-0012](../../adr/0012-adopt-stale-lease-of-live-wrapper.md)、[`supervise`](supervise.md#adopt)）。
`awaiting_integration`のrunはwrapperを問わず引き継ぐ。
それ以外（wrapperが死んだ・黙った、`claimed` / `starting`、`integrating`、leaseなし）は、processが止まっていてleaseが無いかleaseのpidが死んでいればsupervisorが自分で`recover`してtriageにかけ、そうでなければユーザーが`recover`で明示的に復旧する。
