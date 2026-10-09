---
id: design-supervisor-lifecycle-doctor
type: design
title: "`doctor`"
status: current
created: 2026-09-26
scope: runtime
related:
  - adr-t1228-2
  - adr-t2159-1
  - design-supervisor-lifecycle
  - design-supervisor-lifecycle-landing-branch
  - design-supervisor-lifecycle-language
  - adr-0049
  - adr-t906-1
---

# `doctor`

inboxのwatcherの記録と表示（ADR-t906-1決定1の(1)）は、ADR-t906-1を置き換えた[ADR-t1433-5](../../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)が引き継ぐ。
doctorも、supervisorとqueue serviceもcmuxを呼ばない（[ADR-t1433-1](../../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。

ユースケースは`application::health::doctor`で、worktreeとrun directoryとreceiptの有無は`RunFiles`、PIDの生死は`ProcessControl`で見る。

`dagq doctor`は状態を変えずにJSONで報告する。以下は`doctor --full`の内容で、既定の出力はsupervisor 1件・run 1件につき1行相当に圧縮する（ADR-0016の決定4。キー名は変えず省くだけ）: supervisorは`pid`、`alive`、`registered`、`mode`、`workspace_id`、`binary_version`、`parallel`、`parallel_source`、`max_waiting`、`max_waiting_source`、`runtime_planners`、`runtime_planners_source`（値と出どころ。[Run environment](run-environment.md)の`[supervisor]`）、`auto_update`、`providers`（登録の`--claude` / `--codex`の解決先・見つかったか・経路。登録が持たなければnull。[`status`](status.md)と同じ）、`heartbeat_age_secs`、`stale`、`run_ids`、runは`run_id`、`task_id`、`status`、`lease_pid`と`lease_stale`（leaseがなければnull）、`recoverable`、`blocker_count`（`blockers`の件数）、`workspace_id`、`worktree_path`、`progress`（`RunHealth::summary` / `SupervisorHealth::summary`）。

- `supervisors`: `status`と同じ。staleな登録は報告するだけで、`doctor`も`recover`も`integrate`も消さない。
- `runs`: [`status`](status.md)の`runs`と同じrun（未完了run、`in_progress`のtaskの最新の`awaiting_integration` / `needs_session`のrun、leaseを持つ全てのrun。goal 98、task 1520）ごとに、`status`と同じ`progress`（工程・その開始時刻と経過秒・slotの使い方）、`workspace_id`、worktreeとrun directoryとreceiptの存在、`last_error`、そのrunの`lease`（PID、`kill -0`による生存、heartbeatの経過秒数、30秒を超えた`stale`。なければnull）、登録済みwrapper/agentプロセスのPID・生存・heartbeat経過秒数・終了コード。`exited_at`が記録済みのプロセスはPIDが再利用されうるため生存確認せず`alive: null`にする。
- `blockers`: そのrunの`recover`を拒む理由の一覧。そのrunのstatusとprocessとleaseだけを見る。`recover`が受け付けないstatus（`application::health::recover_takes`の外: 未完了runとleaseのある`awaiting_integration`以外。leaseの無い`awaiting_integration`、`needs_session`、`failed` / `interrupted`、`integrated`など）は`run is <status>…; recover takes only …`の1件になり、表示の対象を広げても`recoverable: true`にならない。空なら`recoverable: true`で、reviewの途中でsupervisorが死んだ（leaseがstaleでpidが死んだ）`awaiting_integration`のrunがこれに当たる。
- `run_env`（既定の出力にも出す）: queueが束縛されたrepositoryのmain checkoutの`dagq.toml`の`[run.env]`が名指すプログラム（`RUSTC_WRAPPER`など）を、`doctor`を打ったプロセスのPATHで解決した結果（`config`、`path`、`programs[]`の`variable` / `value` / `resolved`（見つからなければnull）、`missing`）と、supervisorが最後に記録した`run_env_program_missing` / `run_env_program_found`（`supervisor_last`、無ければnull）。`dagq.toml`が読めなければ`error`。`dagq.toml`が無く記録も無いrepositoryでは欄ごと出さない。状態は変えない（[ADR-0049](../../adr/0049-share-compile-cache-across-runs-and-break-down-wait-to-land.md)の決定9、[Run environment](run-environment.md#runenvが名指すプログラムの検査)）。
- `repository`（既定の出力にも出す）: 着地先のbranchとpushのremoteの解決の結果（`branch`・`branch_source`・`remote`・`remote_source`・`remote_exists`・`push`、解決できなければ`error`だけ。明示した`remote`が無くpushするときは欄に`error`を並べる）。queueがcheckoutに束縛されていなければ出さない。repositoryにmain checkoutが無い（bare、またはmain worktreeの分からない`--separate-git-dir`）ときはその理由が`error`になり、`run_env`は出さない（[Run environment](run-environment.md#main-checkoutの決め方)）。規則は[Landing branch](landing-branch.md#upのpreflightとdoctor)（ADR-t615-1）。
- `language`（既定の出力にも出す）: AIが人に向けて書く文の言語の解決の結果（`tag`（未設定ならnull）・`source`（`repository` / `user` / `unset`）・`user_config`のpath・promptに足す`instruction`、書式の誤りなら`error`）。規則は[Language](language.md#upとdoctor)（ADR-t616-2）。
- `d2`（既定の出力にも出す）: `graph --format svg`が使う`d2`と`d2plugin-tala`を、`doctor`を打ったプロセスのPATHで解決した結果（`d2` / `tala`のそれぞれに`path`と、linkならその先の`resolved`。見つからなければnull）と、見つからないものを名指す`error`（揃っていれば出さない）。queueの状態に関わらず出す（[当面の依存図](dependency-diagram.md#描画infrastructured2)、ADR-0077の決定4）。
- `providers`（既定の出力にも出す）: workerのprovider（`claude` / `codex`）を、`doctor`を打ったプロセスのPATHで解決した結果（`provider`・`executable`（見つからなければ名前のまま）・`found`・見つからない理由`error`・このbinaryが動かせる経路`modes`）。supervisorが実際に使うのは登録の`supervisors[].providers`（`up`が固定したpath）で、こちらはその場のPATHの目安。queueの状態に関わらず出す（[Provider lifecycle](../provider-lifecycle.md#workerのproviderと経路)、ADR-t813-2）。
- `roles`（既定の出力にも出す）: worker以外の役割ごとに設定されたproviderと出どころ（ADR-t1063-1の決定1・6、task 1065）。queueが束縛されたmain checkoutの`dagq.toml`の`[roles.<role>]`を読み、役割（`plan_review`・`review`・`recovery`・`goal_review`・`observer`・`throughput_review`・`runtime_planner`・`planner`）ごとに`{"provider", "source", "model", "effort"}`（`source`は`provider`の出どころで`dagq.toml`か`default`、`model` / `effort`は起動に渡す値で、渡さなければnull）を出す（`application::health::roles`）。読めない（`provider = "codex"`を`CODEX_ROLES`（`goal_review`・`review`・`plan_review`・`throughput_review`・`observer`・`recovery`）以外の役割に書いたなど）ときは`error`に理由を足し、役割は既定（Claude）で出す。queueがcheckoutに束縛されていなければ全て既定。読めるqueueのときだけ出す（[Actor model](actor-model.md#provider)）。
- `agents`（既定の出力にも出す）: queueが束縛されたcheckoutのlanding branchの今のcommit（`commit`）の`dagq.toml`で役割の節（`domain::review_subagents::ROLE_SECTIONS`。今は`[review.subagents.<agent>]`だけ）が名指すagentごとの`agent`・`section`・読んだ定義のpath`definition`（`.dagq/agents/<agent>/AGENT.md`、無ければnull）と、設定の誤りの`errors`（定義の無いagent、同じagentを複数の役割の節で名指すこと、定義のfrontmatterの`tools`の宣言の誤り（一覧に無い名前・役割の許可を超える道具・リストでない`tools`。[ADR-t1728-2](../../adr/2026-10-06-t1728-2-agents-declare-their-tools-from-a-runtime-list.md)）。無ければ空の配列）（`application::review::check_agents`、[ADR-t1728-1](../../adr/2026-10-06-t1728-1-agent-definitions-cases-and-eval-as-a-queue-service-use-case.md)）。checkoutかlanding branchか`dagq.toml`が読めない・解釈できなければ`error`だけ。queueがcheckoutに束縛されていないとき、landing branchのcommitに`dagq.toml`が無いかagentを名指していないときは出さない。読めるqueueのときだけ出す。定義の読み方とreviewでの扱いは[Review](review.md#reviewのsubagent)の「設定」。
- `inbox_watcher`（既定の出力にも出す）: inboxのwatcherの有無（`state`（`alive` / `absent`）・`watching`・`last_seen_at`・`absent_secs`・`grace_secs`）。`status`の`inbox_watcher`と同じ判定（`application::inbox_watcher::judge`。heartbeatの新しさで決まり、pidのprocessが居ない・開始時刻が合わない記録は数えない。閾値と猶予と開始時刻の許容の幅は[`events` / `watch`](events-watch.md#inboxのwatcherの記録adr-t906-1)）で、queueのディレクトリのファイルだけを読むので、queueの状態（schemaが拒むときも）に関わらず出す（[ADR-t906-1](../../adr/2026-09-28-t906-1-guarantee-the-inbox-watch.md)）。
- `inbox_guardrail`（既定の出力にも出す）: inboxがguardrailのsettingsで開かれたか。最新の`inbox_opened`による`status`の`inbox_guardrail`と同じ判定（`application::inbox_guardrail::judge`、[ADR-t2159-1](../../adr/2026-10-09-t2159-1-dagq-does-not-use-cmux-and-the-person-opens-the-inbox.md)決定2・5）で、queueが読めるときだけ出す。
- `queue_service`（既定の出力にも出す）: queue serviceの状態。`status`の`queue_service`と同じ（`state`・`socket`・`pid`・`build`・`api_version`・`min_api_version`・`build_matches`・`started_at`・`client_api_version`・`attention`。queueが読めなければ`attention`はnull）。queueのディレクトリのファイルとsocketだけを見るので、queueの状態に関わらず出す（[Queue service](../queue-service.md#statusとdoctor)）。
- `schema`: `migrate --check`と同じqueueのschemaの状態（`schema_version`、`binary_schema_version`、`floor`、`migrate`が適用する`pending`とその`compatible`、このバイナリがそのまま開けるかの`opens`）。既定の出力にも含める。`migrate`が要るqueueでも、floorがこのバイナリを拒むqueueでも報告する（ADR-0045の決定5）。前者のrunとsupervisorはread-onlyのコピーを`migrate`した上で読む。後者は`supervisors`と`runs`を省き、拒む理由を`error`に書く。どちらでもrepositoryの束縛は先に検査する（`SqliteQueue::inspect_read_only`がschemaの状態と、読めるqueueか、floorが拒むときは束縛だけを読む接続と拒む理由を、1つの接続から返す）。

cmux workspaceの存在は確認しない（cmuxなしで動く）。IDを見てユーザーが`cmux workspace list`で確認する。

## CIの見張り（ADR-t1920-1）

[ADR-t1920-1](../../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](ci-watch.md)（task 1921）。結びついたcheckoutの`dagq.toml`に`[ci_watch]`があれば`ci_watch`の欄に`config`（読んだ表）・`gh`（`doctor`のPATHで解決したpathかnull）・`authenticated`（`gh auth status --hostname github.com`が0で終わるか）・`repo`（`<owner>/<name>`かnull）・`supervisor_last`（最後の`ci_watch_unavailable` / `ci_watch_available`の`{kind, created_at, payload}`かnull）を出す。ファイルが読めなければ`{error}`、表が無ければ欄を出さない。状態は変えない。

## sccacheのserver

serverの出どころと失敗の偏りを読む入口は`application::sccache::add_diagnostics`と`report`。
束縛されたcheckoutの設定がsccacheを名指すときだけ表示し、診断の読み取りでserverを起動させない。
出どころを確かめられないことと壊れたserverの判断は分ける（[Run environment](run-environment.md#外部serverと失敗の偏り)）。
