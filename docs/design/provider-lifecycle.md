---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
scope: provider
related:
  - adr-t1857-1
  - adr-t1486-1
  - design-execution-tokens
  - adr-t1570-1
  - adr-t655-1
  - adr-t1228-2
  - adr-t1340-1
  - adr-t1433-2
  - adr-0004
  - adr-t813-1
  - adr-t813-2
  - adr-t2080-1
  - adr-t813-3
  - adr-t1215-1
  - adr-t2086-1
  - adr-t1063-1
  - plan-headless-worker-spike
  - adr-0040
  - adr-0048
  - adr-0027
  - design-supervisor-lifecycle
  - adr-t803-1
---


# Agent provider lifecycle

## 目的

AI agent（Claude Code・Codex）の起動・出力の読み取り・失敗の分類・記録を、providerに依らない契約にまとめる。
applicationはCLIの引数や出力の形式を直接扱わず、providerの違いはadapterだけが持つ。
providerが使えないときは、runやjobを失敗にせず、もう一方のproviderへ移るか待つ（例外は[setup-review](supervisor-lifecycle/setup-review.md#権限)）。
引数・出力の欄・既定値の意味は定義のそばのdoc comment（`src/application/ports/`と`src/infrastructure/`のadapter）が持つ。

## 全体の流れ

```text
claim ──► run ── wrapper ──► claude -p / codex exec（1 turn = 1回の呼び出し）
turnが壁で終わる ──► supervisor ── 切り替え / 同じsessionへもう一度 / queue_hold
headless job ── 役割のprovider ──► 最終の返答・失敗の分類・session
sessionの区間 ── 稼働時間・tokens・model・作業の内訳
turnの出力 ── StepReader ──► step ──► TelemetrySink
```

## 責務と境界

- applicationは`AgentProvider`・`AgentSignals`・`TurnReader`・`StepReader`の4つのport（`src/application/ports/`）だけでproviderにつながる。
- コマンドは値の`CommandSpec`で返し、起動は`Spawner` portが行うので、supervisorとsession wrapperはプロセスを直接扱わない。
- AI actorの起動は全て`ActorExecutor`を通る（[Roles](supervisor-lifecycle/roles.md#actorの起動actorexecutor)）。
- Claude Codeのsettingsはadapter（`src/infrastructure/adapters.rs`）が持ち、headless jobは権限の意図（`JobAccess`）だけを渡す（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)）。
- 切り替えの判断は`domain::provider_switch`と`domain::actor_model`の純粋な関数が持ち、`supervise::execution::provider`は記録と依頼だけを行う。

## 不変条件

- runtimeは人の設定（`~/.claude.json`・`~/.codex/config.toml`・`auth.json`・`CODEX_HOME`）を書かず、runごとの設定は起動の引数とrun dirのファイルだけにする。
- providerの切り替えは使えない理由（`SwitchReason`）のときだけで、ほかの失敗は今までの復旧に回す（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定3）。
- 1つのrunの切り替えは`MAX_PROVIDER_SWITCHES`回まで。
- 区間は開閉を決めたeventと同じトランザクション・同じ時刻で書き、1つの区間は1回だけ閉じる。
- transcriptは書き込みトランザクションの中で読まない。
- 稼働時間・トークン数・modelを読めなくても、run・jobの結果は変わらない。

## headless jobのinterface

worker以外のheadless jobは、権限の意図・promptの渡し方・最終の返答・失敗の分類・sessionだけでproviderにつながる（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)決定2〜4）。
providerは役割ごとに決まる（[Actor model](supervisor-lifecycle/actor-model.md#provider)）。

- 入口: `AgentProvider::headless_command`・`job_reply`・`review_command`（`src/application/ports/execution.rs`）と、`domain::headless_job`の`JobAccess`・`JobFailure`・`JobSession`。
- 約束: promptはargvでなくstdinで渡す（`CommandSpec::stdin`）。
- 落とし穴: 入力の失敗（引数の上限・stdin）はそのjobだけの失敗でproviderを控えない（`job_start_failure`）。
- Claudeのrunのreviewと`setup-review`のjobは、workerが変えられる木と人の設定を読まない（[ADR-t1470-1](../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)、道具の拒否は`adapters.rs`の定数のdoc comment）。
- jobの子プロセスはqueueを直接開かず、クライアントモードの`dagq`を使う（[Queue service](queue-service.md#クライアントモード)）。

### Codexのheadless job

- 入口: `infrastructure::codex::Codex`の`headless_command`・`review_command`・`apply_launch`。
- 役割ごとの流れは各役割の文書の「Codexで動かす」と[ADR-t1207-1](../adr/2026-09-30-t1207-1-codex-run-review.md)が持つ。
- queue serviceへの到達は[Queue service](queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)。
- 落とし穴: runのreviewと`setup-review`のjobだけはworktreeを`untrusted`にする（`codex::distrust_config`、[ADR-t1570-1](../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）。

## Claude sessionの区間

runtimeが起動するsessionとjobは、kindごとの区間として記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)）。

- 入口: どのeventがどの区間を開き閉じるかは`domain::sessions::changes`、書くのは`infrastructure::sessions::follow`。
- hookの区間は`domain::sessions::hook_changes`と`inferred_hook_closes`（hookの側は[plugin-integration](plugin-integration.md#sessionの区間hookadr-0048)、[ADR-t2022-1](../adr/2026-10-08-t2022-1-supervisor-infers-ended-hook-sessions-without-cmux.md)）。
- 落とし穴: hookの区間は`SessionEnd`を取り逃しうるが、経過時間では閉じない（常駐sessionの長いidleがあるため）。

### transcriptと稼働時間

- 入口: `infrastructure::transcripts::ClaudeTranscripts`、前もった読み取りは`infrastructure::sessions::read_before`。
- 約束: transcriptはロックの外で前もって読み解析し、トランザクションの中では解析し直さない（理由は`read_before`のdoc comment）。
- 約束: 区間はその区間のsessionのtranscriptを読み、新しいsessionに切り替えた（`session_renewed`）後に開いた区間は新しいsessionのものを読む（`domain::sessions::SpanContext`の`session`）。
- hookの`SessionEnd`はtranscriptを読まずに閉じ、後でsupervisorが取り込む（[ADR-t655-1](../adr/2026-10-04-t655-1-hook-close-defers-transcript-intake-to-the-supervisor.md)）。
- 落とし穴: `worktime.jsonl`の明細はcommitを確かめてから書く（`sessions::watch_commits`）。

### 作業の内訳

- 入口: 分類は`domain::worktime`（どの規則で分類するかは`domain::command_rules::work_rules`、[計測のコマンドの形と分類](measurement-commands.md)）、落ちたtestの名前は`domain::verify_failure::failed_tests`（読むかは`domain::command_rules::reads_failed_tests`）、Codexの内訳は`domain::turn::codex_span_turns`。
- 落とし穴: コマンドの分類はコマンド位置の語だけで見て、落ちたtestの名前は`[telemetry] failed_tests`を有効にしたときだけ、testを流すforegroundのコマンド（効く規則ではprofileかpjの規則で分類したもの）の出力からだけ読む。

### sessionのstep

- 入口: `AgentProvider::step_reader`が返すproviderの`StepReader`（`infrastructure::claude_steps`・`codex_steps`）で、送り方は[計測](measurement.md#送る口)。
- 約束: stepは長さとturnの始めからの経過と実行したものの形だけを持ち、言ったこと・コマンド全文・トークン数を持たない（トークン数はExecutionの記録だけが持つ）。
- 落とし穴: 出力に時刻が無いので、行の時刻はwrapperが読んだ時刻で、読む間隔の分だけ遅れる。
- 作業の内訳（上）は別の経路のままで、stepはそれを変えない。

### トークン数とコスト

- 入口: `domain::tokens`。
- 1回ごとの記録と今の穴は[Executionのトークン数](execution-tokens.md)。

### 非対話のworkerの区間

非対話の区間は、transcriptを読まずにrunのturnのeventから記録する（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定7）。

- 入口: `domain::turn::HeadlessSpan`。
- turnのトークン数の数え方は[Executionのトークン数](execution-tokens.md#codexの数える元)。

### modelとeffort

- 入口: `domain::tokens::span_models`、読み口は[stats](supervisor-lifecycle/commands.md)（[ADR-0079](../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定7）。
- 約束: plannerの区間は、`submit`が提出したplannerとその開いている区間を記録してproposalに結ぶ。
  `kpi`の計画の品質は、plan reviewの開始の前の最後の提出の区間のmodel / effortをplannerの層にする（`domain::plan_quality::PlannerSubmission`）。

## workerのproviderと経路

workerのproviderはtaskが選び、経路は非対話だけ（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定1、[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
流れは[非対話のworker](supervisor-lifecycle/headless-worker.md)が持つ。

- 入口: `domain::worker`・`WorkerAdapters`・`provider_switch::routes`・`AgentProvider::turn_command`・`TurnReader`、文面は[Prompt](supervisor-lifecycle/prompt.md#経路とproviderごとの文面)。
- 実行ファイルの解決は[Provider executables](provider-executables.md)、`up`での固定は[運用](../development/operations.md)の「`up`のコマンド」。
- 落とし穴: Claudeの`permissions.deny`はコマンドの先頭しか拒めないので、pidで止める規則をpromptでも伝える。
- Codexで動かすtaskは`subagent_review`を要るevidenceから外す（`domain::required_of`）。
- 約束: `in_progress`のtaskのproviderとmodelを変えるのはholdの解除だけ。
  Claudeはturnごとにmodelを受けるので、sessionの残る止まっていない待ちのrun（`needs_session`など）はmodelだけならresumeで続け、Codexはmodelを受けないので`--model`を拒み、providerを変えればrunを終えて成果を引き継ぐ新しいrunにする（`worker_model::check_model`。止まった（`failed`・`interrupted`）runの解除の分かれ方は[needs_session](supervisor-lifecycle/needs-session.md#taskのhold)）。

### Codexの非対話のworker

- 入口: `infrastructure::codex::Codex::turn_command`と`CodexTurnReader`（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)）。
- 約束: `exec resume`は最初の呼び出しの権限を引き継がないので、権限とtrustは毎回同じものを渡す。
- networkは[Run environment](supervisor-lifecycle/run-environment.md#sccacheのserver)のsccacheのserverのため。
- 落とし穴: `pkill` / `killall`を止める主の防御はsandboxで、worktreeのrulesは補助。
- supervisorとwrapperはsandboxの外で動くので、run dirのファイルはlinkを辿らずに読み書きする（`infrastructure::agent_dir`）。
- Codexのsandboxは書き込みとsignalを止めるが隔離ではない（[Security](security.md#codexのworkerのsandbox)）。

## 使えないproviderからの切り替え

[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定2〜6と、切り替えを止める設定の[ADR-t1857-1](../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。

- 入口: `domain::provider_switch`の`SwitchReason`・`ProviderHold`・`wall_move`。
- 人へのaskは`provider_switch::wall_to_raise`、止める設定は`ProviderFallback`。
- headless jobの行き先は`domain::actor_model::job_route`、timer jobの使えない終わりは`domain::actor_model::records_unusable`。
- 約束: Claudeの認証と利用上限の控えは`queue_hold`のaskで、常に人に届く（[Queue hold](supervisor-lifecycle/queue-hold.md)）。
- 切り替えられないときもrunは失敗にせず、wrapperはsessionを終えずに次の依頼を待つ。
- 落とし穴: 切り替えた後は会話が引き継がれず、promptも新しいproviderの文面で書き直す。
- runtimeのplannerは同じ壁で待つが、providerは切り替えない（[Plan planners](supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)）。
- runのreviewとgoal reviewの起動し直しは[Review](supervisor-lifecycle/review.md)と[Goal review](supervisor-lifecycle/goal-review.md)。
- 設定の書式は[Run environment](supervisor-lifecycle/run-environment.md)、集計は[stats と kpi](supervisor-lifecycle/commands.md)。

## 大きなcontextからの新しいsession

[ADR-t2080-1](../adr/2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)、引き継ぎの中身は[ADR-t2852-1](../adr/2026-10-19-t2852-1-handoff-carries-local-check-records-resume-timeout-and-findings-once.md)。

- 入口: 判定は`domain::fresh_session::next_session`、記録と依頼は`supervise::execution::session`、引き継ぎのpromptは`prompt::handoff_text`。
- 約束: 新しいsessionは`provider_switched`と同じ区切りで、providerを変えない。
- 最初のturnはclaimの時のtaskのpromptと引き継ぎで、引き継ぎがtaskのpromptの読む指示を上書きし、文書は要るときだけ開かせる。
- 引き継ぎはreviewの材料と同じ除外で今のHEADのtreeの手元の検証の記録だけを載せ、別のtreeの記録は数だけを書く（receiptの根拠は[ADR-t2589-1](../adr/2026-10-17-t2589-1-local-check-runs-are-recorded-by-project-scripts-and-matched-to-the-receipt-tree.md)の照合のまま）。
- 引き継ぎはresumeの時間の上限の秒を書き、reviewの指摘は送るはずだった依頼の中の1回だけ、known failuresはrunが触るファイルに当たるもの（test binaryとpackageの名前では当てない）だけを載せる。
- 落とし穴: Claude Codeは使われた`--session-id`を拒むので、新しいsessionは新しい名前になる（`domain::turn::session_name`）。

## Trust prompt

Claude Codeのfolder trust dialogがrun worktreeで出るかは、main checkoutのrootが`~/.claude.json`で承認済みかだけで決まる。

- 入口: `adapters.rs`の`claude_trusts_repository`、`up`のpreflight（[`up` / `down`](supervisor-lifecycle/up-down.md)）。
- 約束: runtimeは承認を書かず、`up`が未信頼のrepositoryをerrorで止める（理由はdoc comment）。
- main checkoutの決め方は[Run environment](supervisor-lifecycle/run-environment.md#main-checkoutの決め方)、止まったsessionの扱いは`dagq-recover` skillの`reference/session.md`。

## 起動時のダイアログ

folder trustは上の[Trust prompt](#trust-prompt)、ほかのダイアログの扱いは`adapters.rs`の`stop_hook_settings`のdoc commentが持つ。

## Claude を使わない運転

`--no-claude`はClaudeを起動しない明示的な運転方針で、既定は無効（[ADR-t1204-1](../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)）。

- 入口: `provider_switch::held`・`actor_model::job_start_route`・`HostActorExecutor`。
- 約束: 方針の違うsupervisorは再利用・引き継ぎせず、先にdrainを求める。
- workerもjobもClaudeへ戻さず、Codexで続けられないjobは役割の手動の経路に渡す。
- plannerは起動せず、人が代行する。
