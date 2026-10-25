---
id: design-provider-lifecycle
type: design
title: Agent provider lifecycle
status: current
created: 2026-09-21
scope: provider
related:
  - adr-t1181-1
  - adr-t1857-1
  - adr-t3255-1
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
引数・出力の欄・既定値の意味は定義のそばのdoc commentが持つ。

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
- Claude Codeのsettingsはadapter（`infrastructure::adapters`）が持ち、headless jobは権限の意図（`JobAccess`）だけを渡す（[ADR-t1063-1](../adr/2026-09-29-t1063-1-headless-job-provider-per-role-with-intent-permissions.md)）。
- 切り替えの判断は`domain::provider_switch`と`domain::actor_model`の純粋な関数が持ち、`supervise::execution::provider`は記録と依頼だけを行う。

## 不変条件

- runtimeは人の設定（`~/.claude.json`・`~/.codex/config.toml`・`auth.json`・`CODEX_HOME`）を書かず、runごとの設定は起動の引数とrun dirのファイルだけにする。
- providerの切り替えは使えない理由（`SwitchReason`）のときだけで、ほかの失敗は今までの復旧に回す（[ADR-t813-2](../adr/2026-09-28-t813-2-provider-per-task-and-mutual-fallback.md)決定3）。
- 1つのrunの切り替えは`MAX_PROVIDER_SWITCHES`回まで。
- 区間は開閉を決めたeventと同じトランザクション・同じ時刻で書き、1つの区間は1回だけ閉じる。
- transcriptは書き込みトランザクションの中で読まず、ロックの外で前もって読み解析する（理由は`infrastructure::sessions::read_before`のdoc comment）。
- 稼働時間・トークン数・modelを読めなくても、run・jobの結果は変わらない。

## headless jobのinterface

worker以外のheadless jobは、権限の意図・promptの渡し方・最終の返答・失敗の分類・sessionだけでproviderにつながる（ADR-t1063-1決定2〜4）。
providerは役割ごとに決まる（[Actor model](supervisor-lifecycle/actor-model.md#provider)）。

- 入口: `AgentProvider::headless_command`・`job_reply`・`review_command`と、`domain::headless_job`の`JobAccess`・`JobFailure`・`JobSession`。
- 約束: promptはargvでなくstdinで渡す（`CommandSpec::stdin`）。
- 落とし穴: 入力の失敗（引数の上限・stdin）はそのjobだけの失敗でproviderを控えない（`job_start_failure`）。
- Claudeのrunのreviewと`setup-review`のjobは、workerが変えられる木と人の設定を読まない（[ADR-t1470-1](../adr/2026-10-03-t1470-1-all-claude-run-reviews-load-no-setting-sources.md)、道具の拒否は`adapters.rs`の定数のdoc comment）。

### Codexのheadless job

- 入口: `infrastructure::codex::Codex`の`headless_command`・`review_command`・`apply_launch`。
- 役割ごとの流れは各役割の文書の「Codexで動かす」と[ADR-t1207-1](../adr/2026-09-30-t1207-1-codex-run-review.md)が持つ。
- queue serviceへの到達は[Queue service](queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)。
- 落とし穴: runのreviewと`setup-review`のjobだけはworktreeを`untrusted`にする（`codex::distrust_config`、[ADR-t1570-1](../adr/2026-10-04-t1570-1-codex-run-review-distrusts-the-worktree-project.md)）。

## inboxの起動

`dagq inbox`はproviderの`AgentProvider::inbox_command`でinboxのcommandを作る（選び方と記録の手順は[`up` / `down`](supervisor-lifecycle/up-down.md#dagq-inbox)）。
Claudeのinboxはqueueのdirに書くsettings（inboxのdenyだけ）とpluginで開く。
Codexのinboxは起動の引数だけで開き、何も書かない（[ADR-t1181-1](../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md)決定2）。
Codexのinboxを選べるのはinboxの記録の実装の後で、それまで`lifecycle::inbox_route`が開かずに断る。
使えないCodex（実行ファイルとlogin）も開かずに断り、Claudeに替えない。

- 入口: 引数・hookのtrust・人の設定の確かめは`infrastructure::codex_inbox`（`inbox_command`・`hook_configs`・`LAUNCH_CONFIG`・`check_user_config`）、hookの答えは`application::codex_inbox`、hookを動かすのは`dagq codex-inbox-hook`。
- 約束: 指示はpluginのskillの場所を名指す常に効く指示で渡し、見張りは上限つきの前景の`watch`にする（Codexにはbackgroundのtaskも選択肢のtoolも無い）。
- 約束: hookのtrustは、dagqが起動の引数で渡すhookだけに、その起動だけ与える。
  全hookのtrustを飛ばす引数とsandbox・承認を外す引数は使わない。
  trustはhookごとに決まるので、渡すhookの全部に与える。
  人やrepositoryの`.codex/`のhookは、Codexのtrustの画面に残る。
- 限界: trustの値はCodexの内部の実装（hookの正規化とhash）に依る。
  版が上がって合わなくなったhookは、Codexのtrustの画面に戻り、trustしなければ動かない。
  黙って動くことは無い。
  そのときは、版を名指した既知の値のunit testを測り直す。
  SessionStartが届いても、guardrail・Stop・SessionEndのhookが動いたことは分からない。
- 約束: 人のCodexの設定（`CODEX_HOME`か`~/.codex`の`config.toml`・`auth.json`）と`~/.claude`は書かない（[ADR-t3331-1](../adr/2026-10-25-t3331-1-codex-inbox-writes-no-person-settings-and-refuses-to-open-when-codex-would.md)）。
  TUIが自分で書き足す欄は、引数で止められるものは止め、止められないものはexecの前に人のfileを読むだけで確かめ、欠けていれば開かない。
- 境界: sandboxはどこも書かせず、queue serviceのsocketへの接続だけを足す（`Codex::reach_queue_service`、jobと同じprofile。[Queue service](queue-service.md#codexのsandboxからの到達adr-t1233-5決定4)）。
  inboxの`dagq`はinboxのtokenとともにserviceへ送り、serviceがinboxのprincipalで認可する。
  queueのdirとDBはsandboxから書けないので、DBを開く制御側のコマンド（`up`・`integrate`など）はCodexのinboxからは動かず、人が自分のterminalで打つ。
  hookはsandboxの外のhostで動き、queueはinboxの身元の`dagq`で読む。

## Claude sessionの区間

runtimeが起動するsessionとjobは、kindごとの区間として記録する（[ADR-0048](../adr/0048-record-claude-sessions-by-kind-with-open-and-active-time.md)）。

- 入口: 区間の開閉は`domain::sessions::changes`、書くのは`infrastructure::sessions::follow`。
- hookの区間は`domain::sessions::hook_changes`と`inferred_hook_closes`（hookの側は[plugin-integration](plugin-integration.md#sessionの区間hookadr-0048)、[ADR-t2022-1](../adr/2026-10-08-t2022-1-supervisor-infers-ended-hook-sessions-without-cmux.md)）。
- 落とし穴: hookの区間は`SessionEnd`を取り逃しうるが、経過時間では閉じない（常駐sessionの長いidleがあるため）。

### transcriptと稼働時間

- 入口: `infrastructure::transcripts::ClaudeTranscripts`、新しいsessionの後の区間が読むtranscriptは`domain::sessions::SpanContext`の`session`。
- hookの`SessionEnd`はtranscriptを読まずに閉じ、後でsupervisorが取り込む（[ADR-t655-1](../adr/2026-10-04-t655-1-hook-close-defers-transcript-intake-to-the-supervisor.md)）。
- 落とし穴: `worktime.jsonl`の明細はcommitを確かめてから書く（`sessions::watch_commits`）。

### 作業の内訳

- 入口: 分類は`domain::worktime`（規則は[計測のコマンドの形と分類](measurement-commands.md)）、落ちたtestの名前は`domain::verify_failure::failed_tests`と`command_rules::reads_failed_tests`、Codexの内訳は`domain::turn::codex_span_turns`。

### sessionのstep

- 入口: `AgentProvider::step_reader`が返す`StepReader`、送り方は[計測](measurement.md#送る口)。

### トークン数とコスト

- 入口: `domain::tokens`、1回ごとの記録と今の穴は[Executionのトークン数](execution-tokens.md)。

### 非対話のworkerの区間

非対話の区間は、transcriptを読まずにrunのturnのeventから記録する（ADR-t813-2決定7）。

- 入口: `domain::turn::HeadlessSpan`、トークン数の数え方は[Executionのトークン数](execution-tokens.md#codexの数える元)。

### modelとeffort

- 入口: `domain::tokens::span_models`、読み口は[stats](supervisor-lifecycle/commands.md)（[ADR-0079](../adr/0079-record-task-weight-predictions-and-trial-model-effort-selection.md)決定7）。
- 計画の品質が使うplannerの区間は`domain::plan_quality::PlannerSubmission`。

## workerのproviderと経路

workerのproviderはtaskが選び、経路は非対話だけ（ADR-t813-2決定1、[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）。
流れは[非対話のworker](supervisor-lifecycle/headless-worker.md)が持つ。

- 入口: `domain::worker`・`WorkerAdapters`・`AgentProvider::turn_command`・`TurnReader`、文面は[Prompt](supervisor-lifecycle/prompt.md#経路とproviderごとの文面)。
- 実行ファイルの解決は[Provider executables](provider-executables.md)、`up`での固定は[運用](../development/operations.md)の「`up`のコマンド」。
- 落とし穴: Claudeの`permissions.deny`はコマンドの先頭しか拒めないので、pidで止める規則をpromptでも伝える。
- 約束: Claudeのturnもpromptをargvに載せない（理由と渡し方は`AgentProvider::turn_command`のdoc comment）。
- 約束: `in_progress`のtaskのproviderとmodelを変えるのはholdの解除だけ（`worker_model::check_model`、[needs_session](supervisor-lifecycle/needs-session.md#taskのhold)）。

### Codexの非対話のworker

- 入口: `infrastructure::codex::Codex::turn_command`と`CodexTurnReader`（[ADR-t813-3](../adr/2026-09-28-t813-3-codex-worker-permissions.md)）。
- 約束: 権限とtrustは毎回同じものを渡す（理由は`infrastructure::codex`のmodule doc comment）。
- networkは[sccacheのserver](supervisor-lifecycle/run-environment.md#sccacheのserver)のため。
- 落とし穴: `pkill` / `killall`を止める主の防御はsandboxで、worktreeのrulesは補助で、sandboxは隔離ではない（[Security](security.md#codexのworkerのsandbox)）。
- supervisorとwrapperはsandboxの外で動くので、run dirはlinkを辿らずに読み書きする（`infrastructure::agent_dir`）。

## 使えないproviderからの切り替え

ADR-t813-2決定2〜6と、切り替えを止める設定の[ADR-t1857-1](../adr/2026-10-06-t1857-1-provider-fallback-can-be-turned-off-for-workers-and-jobs.md)。

- 入口: `domain::provider_switch`（`SwitchReason`・`ProviderHold`・`wall_move`・`wall_to_raise`・`ProviderFallback`）、jobは`domain::actor_model`の`job_route`・`records_unusable`。
- 約束: Claudeの認証と利用上限の控えは`queue_hold`のaskで、常に人に届く（[Queue hold](supervisor-lifecycle/queue-hold.md)）。
- 控えの間のresumeの待ちは[needs_session](supervisor-lifecycle/needs-session.md)の「providerだけの試行」。
- 落とし穴: 切り替えた後は会話が引き継がれない（`domain::turn::new_session_prompt`）。
- runtimeのplannerは同じ壁で待つが、providerは切り替えない（[Plan planners](supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)）。
- reviewの起動し直しは[Review](supervisor-lifecycle/review.md)と[Goal review](supervisor-lifecycle/goal-review.md)、設定は[Run environment](supervisor-lifecycle/run-environment.md)。
- 境界: inboxは切り替えと控えの外にある（[ADR-t1181-1](../adr/2026-10-24-t1181-1-codex-inbox-provider-selection-gaps-unavailability-and-records.md)決定4）。
  runtimeはinboxを起動しないので、控えはinboxを止めず、inboxはClaudeでしか動けない役割に数えない。
  Claudeの控えで`queue_hold`のaskを開く条件とCodexだけの控えで開かないことはinboxのproviderで変わらず、どちらのinboxもそのaskに答えられる。

## 大きなcontextからの新しいsession

[ADR-t2080-1](../adr/2026-10-08-t2080-1-fresh-session-on-send-back-and-resume-when-peak-context-exceeds-threshold.md)、引き継ぎの中身は[ADR-t2852-1](../adr/2026-10-19-t2852-1-handoff-carries-local-check-records-resume-timeout-and-findings-once.md)。

- 入口: 判定は`domain::fresh_session::next_session`、記録と依頼は`supervise::execution::session`、引き継ぎは`prompt::handoff_text`と`HandoffMaterial`、最初のturnとsessionの名前は`domain::turn`の`new_session_prompt`・`session_name`。
- 約束: 新しいsessionは`provider_switched`と同じ区切りで、providerを変えない。

## Trust prompt

Claude Codeのfolder trust dialogがrun worktreeで出るかは、main checkoutのrootが`~/.claude.json`で承認済みかだけで決まる。

- 入口: `adapters.rs`の`claude_trusts_repository`、`up`のpreflight（[`up` / `down`](supervisor-lifecycle/up-down.md)）。
- 約束: runtimeは承認を書かず、`up`が未信頼のrepositoryをerrorで止める（理由はdoc comment）。
- main checkoutの決め方は[Run environment](supervisor-lifecycle/run-environment.md#main-checkoutの決め方)、止まったsessionの扱いは`dagq-recover` skillの`reference/session.md`。

## 起動時のダイアログ

folder trust以外のダイアログの扱いは`adapters.rs`の`stop_hook_settings`のdoc commentが持つ。

## Claude を使わない運転

`--no-claude`はClaudeを起動しない明示的な運転方針で、既定は無効（[ADR-t1204-1](../adr/2026-09-30-t1204-1-explicit-no-claude-operation.md)）。

- 入口: `provider_switch::held`・`actor_model::job_start_route`・`HostActorExecutor`。
- 約束: 方針の違うsupervisorは再利用・引き継ぎせず先にdrainを求め、workerもjobもClaudeへ戻さず、Codexで続けられないjobは役割の手動の経路に渡し、plannerは起動せず人が代行する。
- `dagq inbox`はClaudeのinboxを開かず、Codexで開くか、開けなければ理由を出す（[`up` / `down`](supervisor-lifecycle/up-down.md#dagq-inbox)）。
