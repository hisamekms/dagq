---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
updated: 2026-10-04 # task 1582
last_verified: 2026-10-04 # task 1582
scope: system
related:
  - adr-t1582-1
  - adr-t1394-1
  - adr-t1394-2
  - adr-0001
  - adr-0002
  - adr-0003
  - adr-0004
  - adr-0005
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0010
  - adr-0022
  - adr-0044
  - adr-0013
  - adr-0028
---

# System overview

ステップ7の時点でRust CLI、SQLiteキュー、依存が解けたtaskを上限付き並列で実行してreceiptを検証する常駐supervisor（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)）、検証済みrunを最新mainへrebase・再検証して1 commitにsquashしmainへ着地させるmerge queue `integrate`（[ADR-0008](../adr/0008-merge-queue-squash-landing.md)）、`doctor`/`recover`、cmux adapter、Claude provider、Claude Code pluginを実装済み。以下の構成図のうち、Codex providerは後続実装。

コードは単一Cargo package内で、[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)のレイヤー構成に分けている（crateは分けない。外部公開APIとschemaは再編の前と同じ）。

- `domain`（`src/domain/`）: 集約とドメインの判断。I/O・DBライブラリ・`anyhow`に依存しない。集約は`Task`（`domain::task`）・`Goal`（`domain::goal`）・`TaskRun`（`domain::run`）の3つで、フィールドは非公開、新規作成は`new`、DBからの復元は`restore`、状態の変更は`Result<_, DomainError>`を返すコマンド関数（[domain-model](domain-model.md)）。IDとcommitはnewtype（`domain::ids`の`TaskId`・`GoalId`・`RunId`・`CommitSha`）、業務上の拒否は`DomainError`（`domain::error`）。読み取り専用のview型は`domain::views`、入力型は`domain::input`、scopeの判定は`domain::scope`、statsの集計は`domain::stats`。runの`run_events`の畳み込み（`RunHistory`）と、resume・reviseの上限・承認・attentionで決まる判断（`decide_conflict`・`decide_revise`・`after_validation`・`run_attention_of`など）は`domain::run`、`run_events`のkind名の定数は`domain::event_kind`。
- `application`（`src/application/`）: ユースケースとport。portは`application::ports`のtraitで、queueの`Queue`（`TaskStore`、runの6つのport、`AskStore`などを合わせたもの。threadごとの接続は`QueueOpener`。runのportは[ADR-0032](../adr/0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)の分類に沿って、runの集約の保存と遷移の`RunTransitions`（claimから着地・後始末まで）と`RunRecovery`（triage・resume・adopt）、協調状態の`RunCoordination`（lease・heartbeat・supervisorの登録と引き継ぎ・wrapperとagentのprocess・backendのslot・repositoryの束縛）と`SessionRegistry`（`session_workspaces`・planner・Claude sessionの区間）、runとeventの読み取りの`RunLog`（run・eventの一覧とrun遷移の外で記録するevent）、レポートの`QueueRecords`（report・KPIの目標割れ・forecast・`stats`が引く対応表）に分かれ、どれも30メソッド以下。一部のportだけを使うユースケース（`rebind`など）はそのportだけを受け取るので、test doubleもそのportだけを実装すればよい）、Gitの`Repository`と`MainRemote`、検証コマンドの`Verifier`、cmuxの`WorkspaceBackend`、Claude Codeの`AgentProvider`（コマンドは`CommandSpec`で返す）とその画面のダイアログ・idle markerの内容を読む`AgentSignals`、launchdの`LaunchAgent`、PIDの生死とsignalの`ProcessControl`、子プロセスの`Spawner`、runのファイルの`RunFiles`、時刻とIDの`Clock`・`IdGenerator`（組は`Generators`）。ユースケースは`supervise`（supervisorのループ。`supervise/mod.rs`がループとslotの状態機械、`session`・`exit`・`jobs`・`landing`・`revise`・`resume`・`triage`・`adopt`がphaseごとのwatchとその処理、`idle`がidle markerの判定）、`session`（session wrapper）、`integrate`（着地）、`health`（`status`・`doctor`・`recover`とattention、文字列の切り詰め`truncate`とinbox向けのeventの圧縮`compact_event`）、`lifecycle`（`up`・`down`。LaunchAgentのplistの内容`LaunchAgentSpec`もここ）、`review`・`rebind`・`stats`・`ask`、`prompt`（worker・inbox・plannerのprompt、headlessのreview・triageのprompt、sessionに打ち込むresume・reviseの依頼文）、`recording`（cmux呼び出しの失敗の記録）、`naming`（workspaceの名前とshellの引用）。applicationはinfrastructureの型を名指ししない。進行と診断のメッセージはportを通さず`tracing`のマクロで出し、subscriberは起動部分が組み立てる（[ADR-0033](../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)）。
- `infrastructure`（`src/infrastructure/`）: portの実装。`sqlite`・`runtime_store`（runのportごとのmodule: `transitions`・`recovery`・`coordination`・`session_registry`・`run_log`・`queue_records`・`ask_store`と、共有のhelperを持つ`mod.rs`）・`asks`（`SqliteQueue`。SQLiteとの変換は`sql_ids`、supervisorのthreadごとの接続は`SqliteOpener`）、`adapters`（`GitRepository`、`Cmux`、`ClaudeCode`、`SystemProcesses`）、`claude`（`ClaudeCode`の`AgentSignals`: Claude Codeの画面のダイアログの判定`detect_prompt`とStop hookの入力の読み取り）、`launchd`（`Launchctl`）、`process`（`LocalSpawner`）、`run_files`（`LocalRunFiles`）、`telemetry`（`tracing`のsubscriber。queueの`logs/`へのJSON Linesとstderr、panic hook）、`run_env`（`ShellVerifier`）、`clock`（`SystemClock`と`UuidGenerator`）、`location`（cwdからのqueueの解決、[ADR-0006](../adr/0006-queue-per-repository.md)）。
- 組み立て: `compose`（`src/compose.rs`）がコマンドごとの入口で、queueとrepositoryを開き、portの実装を作ってユースケースに注入する（`supervise`・`session`・`integrate`・`status`・`doctor`・`recover`・`ask`・`up`・`down`・`review`・`rebind`・`stats`）。one-shotの入口（`integrate`・`status`・`doctor`・`recover`・`stats`・`rebind`・`up`・`down`）は`compose::OneShot`のメソッドで、`OneShot`が持つ`Generators`を開いたqueue（`with_generators`）とユースケースの両方に渡す。同名の自由関数はsystemの`Generators`で作った`OneShot`を呼ぶテスト向けの入口。`main`（`src/main.rs`）はqueueの位置の解決、`Generators`（`SystemClock`と`UuidGenerator`）の1回の組み立てと注入（`OneShot`、`SuperviseOptions.generators`、自分が開くqueue）、CLIの解析と役割による拒否、`compose`の入口か`TaskStore`のportの1回の呼び出し、JSONの出力とexit codeだけを行う。
- レイヤーの外: `view`（`show`・`goal show`の圧縮した出力）、`watch`（`events`と`watch`。attentionの導出は`application::health`）、`observer`（observerのjob。queueを直接開く）。applicationはこれらを参照せず、`view::truncate`と`watch::compact_event`は`application::health`の再輸出。`runtime`と`lifecycle`は再編前の名前をテストのために再公開するだけのモジュール。

利用方法は[README](../../README.md)を参照。

dagqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをcmux workspaceとGit worktreeで実行するRust runtimeである。

> **予定（goal 92）**: cmuxはinboxだけが使う形に移る（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。cmuxを呼ぶのは`up`がinboxのworkspaceを開く・確かめる・閉じることとinboxのsessionの中の操作だけになり、worker・runtimeのplanner・supervisor・queue service・observer・jobはcmuxを呼ばない。対話の経路（workerの`--interactive`とruntimeのplannerの対話）は廃止し（[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、非対話のsession wrapperはbackgroundのprocessだけで動き（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、supervisorはin-cmux modeなしでlaunchdで常駐し（[ADR-t1433-4](../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）、runtimeはinboxのterminalに打ち込まない（[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)）。この文書の対話・runのworkspace・in-cmux modeの記述は、goal 92の後続のtaskが実装するまでの今の姿である。

## 用語集

役割は5つで、ADR-0024の決定1で確定し、[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1が引き継いだ: supervisor、worker、planner、inbox、observer。このうち`DAGQ_ROLE`で名乗るworkspaceを持つのはsupervisor（in-cmux modeのとき）、worker、planner、inboxで、cmux workspaceのtitleは`[<repo>]<role>`（workerは`[<repo>]worker#<task-id> - <task title>`、[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)）。plannerは`[<repo>]planner#<planner-id>`で、同時に開いている複数のplannerを見分ける。`up`が開くのはsupervisor（in-cmux mode）とinboxだけで、plannerはproposalごとのオンデマンドのworkspaceとしてruntimeが立てる（ADR-0044の決定1・6。task 277で実装。[supervisor-lifecycle](supervisor-lifecycle/plan-planners.md#plan--planners)）。人が開くplannerは[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)で廃止した: 人はinboxに計画を頼み、inboxが人の言葉を計画の依頼として記録し、supervisorが依頼ごとにruntimeのplannerを立てる（task 1395）。`dagq plan`は何も開かず、その案内を付けて拒む（task 1399）。廃止前に人が`dagq plan`で開いたplannerは、終わるまで動いてsupervisorが閉じる。runtimeのplannerの経路は`dagq.toml`の`[roles.runtime_planner]`で対話（workspaceのterminal）と非対話（workerと同じturn）から選ぶ（[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)、task 1396。[runtimeのplannerの経路](supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)）。goal 92の予定では対話の経路を廃止し、runtimeのplannerは非対話だけで、supervisorから切り離したbackgroundのwrapperで動く（[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)、[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)、未実装）。planの検査はsupervisorが起動するheadlessのplan review jobが行い、taskを`ready`にする（goal 29の後続taskが実装する）。observerとreview / triageのjobはworkspaceを持たないheadlessの`claude -p`で、`DAGQ_ROLE`はobserverが`observer`、jobがそれぞれ`review-job`・`recovery-job`・`plan-review-job`・`goal-review-job`（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)、[Roles](supervisor-lifecycle/roles.md#actors)）。

| 用語 | 指すもの | 旧称 |
| --- | --- | --- |
| **supervisor** | runtimeの`dagq supervise`プロセス。依存が解けたtaskをclaimし、runごとにworktreeとcmux workspaceを作ってworkerを起動し、receiptを検証し、runごとのheadlessのjob（review、triage）を起動してそのverdictで着地・差し戻し・resume・retryを行い、`needs_session`のrunをresumeし（3回で解消しなければ人へのaskにする）、後始末をする（[ADR-0003](../adr/0003-supervisor-owns-lifecycle.md)、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)、[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、ADR-0044）。launchdのLaunchAgentとして、またはin-cmux modeで常駐する。 | "SV"（supervisorの略） |
| **worker** | runごとにsupervisorが起動するClaude（将来はCodex）のsession。割り当てられたworktreeの中だけで作業し、commitしてreceiptを書く。判断が要るときは`worker_question`のaskを登録して止まる。 | agent session、run session |
| **planner** | goal / taskを書いてproposalとしてsubmitし、follow_upのdraft taskの採否とobserverのfindingの扱いを人と決め、goalをcloseするsession（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0044の決定1・6）。常駐せず、proposalごとのオンデマンドのworkspace `[<repo>]planner#<planner-id>`で動く。runtimeが立てる（revise・draft・finding・inboxからの計画の依頼。複数同時に開ける）。人が開く`dagq plan`は廃止し、inboxに頼む案内を付けて拒む（ADR-t1394-1、task 1399）。人の指示で`up` / `down`も打つ。runtimeのplannerの経路は`[roles.runtime_planner]`で対話と非対話（turnごとの呼び出し）から選ぶ（ADR-t1394-2、task 1396）。決めきれない判断は`planner_question`でinboxに上げる（ADR-t1394-1）。予定（goal 92、ADR-t1433-2・ADR-t1433-3、未実装）: 経路は非対話だけになり、backgroundのwrapperで動く。 | （新設） |
| **inbox** | 人に届くものすべての窓口になる常駐session（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0044の決定6）。openなaskを人に見せてanswerを書き戻し、それ以外のattention（回答済みのask、止まったsupervisor、失敗したreview / triage、pushの失敗）を人に知らせ、人の指示があるときだけ`dagq-recover`の手順（`up` / `down`、手でのreviewと`integrate`、`recover`、run workspaceへのキー送信）を実行する。自分では判断しない。人が頼んだ計画は人の言葉のまま計画の依頼として記録してruntimeのplannerに移譲し（`dagq request add`、ADR-t1394-1。CLIと依頼のplannerはtask 1395で実装）、結末（`proposed` / `declined` / `exhausted`）を人に伝える（`dagq-inbox` skillの手順は後続のtaskが書く）。`up`が`[<repo>]inbox`のworkspaceに`inbox_prompt`付きで開く。 | （新設） |
| **observer** | supervisorのtimerで定期起動するheadlessのjob（ADR-0044の決定4）。状態は変えない。`stats`、閉じていないfinding、直近のnoteを読み、findingの記録と更新（決定18）とfindingに紐づく`blocked`のask（決定23）だけを書く。noteとdraft goalは書かない（task 292）。変化の無いときに起動しないこと、proposalを求める印からの経路などの決定19〜22はgoal 31の後続taskが実装する。 | （新設） |

**退役した役割。** ADR-0010からADR-0023までの記述に出てくる常駐のClaude Code session「メンテナー」（英字表記の役割名。`up`が`[<repo>]`＋その名のworkspaceを開き、`DAGQ_ROLE`にその名を持っていた）は、ADR-0024で退役し、その決定は[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1が引き継いでいる。既存のADRは書き換えないので、そこでのメンテナーの仕事は次のとおり読み替える: レビューと着地はsupervisorのreview job、失敗runの扱い（recoverしてready / cancel）はsupervisorのtriage job、3回resumeして解消しない`needs_session`と作業中のdialog（`prompt_waiting`）はinbox宛てのask、継続的な監視と改善提案はobserver、人への相談とanswerに従う操作・`up` / `down` / 固定バイナリの更新はinbox（またはplanner）のsessionから人の指示で行う。`up`はそのworkspaceを開かず、queueに記録されたそのworkspaceの行を忘れる（workspace自体は人が閉じる）。

既存のADR（0001〜0009）に残る旧称もこの表で読み替える。runtimeのCLI名（`supervise`）と`supervisors`表は変えない。

```text
CLI / Claude plugin / Codex plugin
                │
                ▼
        SQLite task queue
                │
                ▼
           supervisor
          ┌─────┴─────┐
          ▼           ▼
      cmux adapter  provider
          │       ┌───┴───┐
          ▼       ▼       ▼
       workspace Claude  Codex
          │
          ▼
       Git worktree
```

テストは3層に分ける。`tests/it/queue_*.rs`（機能のまとまりごとのファイルで、共通のhelperは`tests/common/queue.rs`）・`tests/it/cli_*.rs`（コマンドのまとまりごとのファイルで、共通のhelperは`tests/common/cli.rs`）・`tests/it/location.rs`はSQLiteキュー、CLI、cwdからのqueue解決を、`tests/it/runtime_*.rs`（機能ごとのファイルで、共通のfixtureは`tests/it/runtime_support/`）はproviderとworkspaceをテストダブルに差し替えたsupervisor/wrapperを、`tests/it/lifecycle_*.rs`（`up`・`up --in-cmux`・`down`・cmux adapter・入れ替え・planner・`install`のまとまりごとのファイルで、共通のfixtureとfakeは`tests/common/lifecycle.rs`）はlaunchd・cmux・signalをfakeに差し替えた`up` / `down`を、cmuxなしで検証する（行カバレッジ80%の対象）。これらは`tests/e2e.rs`と`tests/plugin.rs`を除いて1つのtest binary `it`（`tests/it/main.rs`がファイルごとに`mod`を並べ、`tests/common`を`#[path]`で読む）にまとめ、tests/commonのcompileとlinkを1回にする（[ADR-0078](../adr/0078-one-integration-test-binary.md)。`Cargo.toml`は`autotests = false`で`[[test]]`は`it`・`e2e`・`plugin`の3本）。`tests/e2e.rs`は実バイナリ・実Git・実cmuxで、使い捨てrepositoryをcwdにして（`XDG_DATA_HOME`は一時dir）`init → add → ready → supervise --once → integrate`（squash着地、worktree削除）を1件と、2件同時（`--parallel 2`、依存taskは着地後の2回目のpassで新しいmainから、`integrate --next`のFIFO、同じファイルを書いた2件目の`needs_session`とテストがセッション役で解消してからの着地）で通し、Claudeの代わりに、promptからreceipt pathを読み取って変更・commit・receipt提出を行い、Stop hook相当のidle markerを書いてから端末の`/exit`を待つstubスクリプトを`--claude`に渡す。supervisorの`/exit`送信とworkspace closeも実cmuxで通る。cmuxが必要なので`#[ignore]`で、`cargo test --locked --test e2e -- --ignored`で実行する。cmuxが起動する`runner`は`LLVM_PROFILE_FILE`を継承せずworktreeに`.profraw`を書いてclean判定を落とすため、`cargo llvm-cov -- --include-ignored`では通らない。カバレッジは通常のテストだけで測る。e2eの後片付け（workspace・workspace groupのguard、`TempDir`）はDrop頼みで、testのプロセスがSIGTERM / SIGKILLで終わると走らないので、各testのfixtureは始める前に前のe2eの残骸をsweepする。e2eの上限の無い待ち（`dagq`・cmux・gitの子プロセス、threadのjoin）は`tests/common`の`within`（`bounded_output`）で包み、fixtureはtest全体の上限（`SUPERVISE_TIMEOUT`などのdeadlineより長い30分）を持つ。時間切れのexitはDropを飛ばすので、fixtureのworkspace groupの削除とtestが起動したsupervisorのkillは`common::on_timeout`にも登録し、時間切れを見たmonitorが新しい順にそれぞれ上限（60秒）付きで実行してからexit 101にする（task 440。登録はguardのDropで外れる）。fixtureは一時dirの`e2e-owner`にflockを取って持ち続け（ロック済みのfileをrenameで置くので、ロック前のownerが見えることはない）、sweepはlockを取れたdir（ownerのプロセスが死んでいる）と、`e2e-owner`が無くfixtureの形（`claude-stub`と`data/`）で1時間以上古いdirだけを対象にする。lockを持ったまま、そのdirにある実行ファイル（runnerの複製）、`/bin/sh <dir>/claude-stub`、`--db`か`--claude`にdirの中を渡された`dagq`（一時queueのsupervisor）のプロセス（`ps`はargvの区切りを失うので、この形だけを見て、promptの中の語では選ばない）をSIGTERM（3秒後にSIGKILL）で止め、そのqueue hashをexternal IDに持つworkspace groupを`--close-workspaces`で消し、`DAGQ_QUEUE`か`E2E_SHARED`がdirを指すworkspaceを閉じ、dirを消す。dirがもう無いe2eの残りも閉じる: `DAGQ_QUEUE`か`E2E_SHARED`が`$TMPDIR`直下の存在しない`.tmp*`の中を指すworkspace（pin済みも）をunpinして閉じ、その`DAGQ_QUEUE`のqueue hashのgroupも消す（guardから漏れたworkspaceは、dirが消えた後はこの形でしか見つからない）。workspaceは`list-windows`で全windowを列挙し、windowごとの`workspace list --window`をまとめた一覧（runtimeの`merged_workspace_listing`と同じ）から探すので、別のwindowへ移されたものも閉じる。一覧が取れなければ何も閉じずにstderrに理由を出す。並行する別のe2eのdirはownerがlockを持っているので触らず、本番queueのworkspaceとgroupは条件に当たらない（本番の`DAGQ_QUEUE`はdata homeの下）。片付けたものは`e2e sweep:`で始まるstderrの行に残る。testは`up`などworkspaceを開くコマンドの出力を受け取ると、assertより前に、出力が名指すworkspaceのIDをすべてguardに積む（JSONでなければ出力の中のUUIDを積む）。guard・一覧・sweepは`tests/e2e/cleanup.rs`（`tests/e2e.rs`が`#[path]`で読む）にある。sweepのe2eは既定では今のwindowで確かめ、`DAGQ_E2E_WINDOWS=1`のときだけ新しいwindowに移して確かめる（windowの開閉の間はcmuxがそのwindowの`workspace list`を`TabManager not available`で失敗させ、runtimeの全windowの一覧も失敗するので、既定のe2eでwindowを開くと並行するe2eやrunを落とす）。ただしsweepのe2e（`the_sweep_closes_workspaces_left_in_any_window_after_their_fixture_dir_is_gone`）は[ADR-t1582-1](../adr/2026-10-04-t1582-1-temporarily-leave-broker-and-cmux-only-e2e-cases-out.md)で本文を残したまま`#[cfg(any())]`で期間限定で登録から外れており、`--ignored`でも関門でも流れない。task 1440がこのケースとcfgを削除して終える（fixtureの始めのsweepはそのまま動く）。

タスクは`draft | submitted | ready | in_progress | completed | canceled`を持つ。`submitted`はplan review待ちで（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定8）、plannerが`submit`でproposalにして出す。`ready`にするのはplan reviewの経路と、人の`ready --bypass-review`と、失敗したrunの再試行だけ。`ready`で依存先がすべて`completed`のタスクだけがschedulerの起動候補になる。詳細な実行状態はTaskRunに保存する。

supervisorはagentの完了レシート、コミット、テスト、worktreeのclean状態を確認してからworkspaceを削除する。worktreeとbranchは`integrate`がmainへ着地させるまで残し、着地後に`integrate`が削除する。

### Gitの実行fileの解決

`infrastructure::git_binary::git_executable`はprocessごとの`LazyLock`で、最初のPATHから`git`の絶対pathを一度だけ解決する。`GitRepository`、`git_common_dir`・`main_checkout_of`・commit messageの読取り、binary更新とbrokerのcommitter読取り、およびroot packageのtestのgit helperはこの解決を共有する。macOSで解決先が`/usr/bin/git`（Command Line Tools / Xcodeのxcrun shim）のときだけ、`/usr/bin/xcrun --find git`で選択中のdeveloper toolsの実体を求める。絶対pathで、正規化できる実行可能な通常fileならそれを使う。Homebrew・Linuxなどのgitには追加の探索をしない。xcrunの起動失敗・非zero終了・30秒の探索期限・不正な出力・実体の不在や実行権限の不足では、最初に見つけたgitへfallbackする。最初のPATHにもgitが無ければ従来どおりエラーになる。解決結果（fallbackとエラーを含む）はprocessの寿命中固定で、PATHやdeveloper toolsを変えた場合は次のprocessから反映される。git操作自体の引数・結果・timeout・閾値は変えない。
