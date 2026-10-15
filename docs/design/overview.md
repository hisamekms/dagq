---
id: design-overview
type: design
title: System overview
status: current
created: 2026-09-21
scope: system
related:
  - adr-t1545-1
  - design-architecture
  - design-measurement
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

コードは単一Cargo package内で、[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)のレイヤー構成に分けている（crateは分けない）。
責務はレイヤーとコンテキスト（計画管理・実行と着地・観測と分析・host運用）の2軸で分ける（[ADR-t1545-1](../adr/2026-10-04-t1545-1-split-the-runtime-by-layer-and-context.md)がADR-0013決定1をamends）。
下の箇条はレイヤーの軸で、contextの目的と禁止する依存の規則（L1〜・C1〜・X1〜）と検査の範囲は[レイヤーとコンテキストの境界](architecture.md)が持つ。
計測（観測と分析）の作り直し（記録の論理ストアのSSOTとビューの区分・区間とタグ・台帳・送る口。一部だけ実装済み）は[計測](measurement.md)が持つ。

- `domain`（`src/domain/`）: 集約とドメインの判断。
  I/O・DBライブラリ・`anyhow`に依存しない（規則は[architecture](architecture.md)のL2）。
  集約は`Task`（`domain::task`）・`Goal`（`domain::goal`）・`TaskRun`（`domain::run`）の3つで、フィールドは非公開、状態の変更は`Result<_, DomainError>`を返すコマンド関数（[domain-model](domain-model.md)）。
  IDとcommitはnewtype（`domain::ids`）、業務上の拒否は`DomainError`（`domain::error`）。
  runの`run_events`の畳み込み（`RunHistory`）と、resume・reviseの上限・承認・attentionで決まる判断は`domain::run`、`run_events`のkind名の定数は`domain::event_kind`。
- `application`（`src/application/`）: ユースケースとport。
  portは`application::ports`のcontextごとのmoduleのtrait（[architecture](architecture.md)のC8）で、queueの`Queue`はqueueのportを合わせたもの、threadごとの接続は`QueueOpener`が開く。
  runのportは[ADR-0032](../adr/0032-classify-records-into-domain-events-diagnostics-coordination-and-bodies.md)の分類に沿って分かれ、一部のportだけを使うユースケースはそのportだけを受け取るので、test doubleもそのportだけを実装すればよい。
  supervisorのループのユースケースは`supervise`（[supervise](supervisor-lifecycle/supervise.md)）にある。
  applicationはinfrastructureの型を名指ししない（[architecture](architecture.md)のL3）。
  進行と診断のメッセージはportを通さず`tracing`のマクロで出し、subscriberは起動部分が組み立てる（[ADR-0033](../adr/0033-one-tracing-pipeline-with-local-json-lines-and-optional-otlp.md)）。
  observer・`watch`・スループットの見直しのjobもapplicationのユースケースで、それだけに固有のportはユースケースと同じmoduleに、queueのportは`application::ports`に置く。
  CLIとqueue serviceのqueueの読み取りは`application::queue_reads::answer`がcontextごとのmoduleに振り分け、各moduleは要るportだけを問い合わせ、hostと組んで作る読み取りはmoduleごとのtrait越しに受け取る。
- `infrastructure`（`src/infrastructure/`）: portの実装。
- 組み立て: `compose`がコマンドごとの入口で、queueとrepositoryを開き、portの実装を作ってユースケースに注入する。
  配線はcontextごとに`src/compose/`に分かれ、入口は`src/compose.rs`が再公開する（[architecture](architecture.md)のL7・L9）。
  one-shotの入口は`compose::OneShot`のメソッドで、`OneShot`が持つ`Generators`を開いたqueueとユースケースの両方に渡す。
  同名の自由関数はsystemの`Generators`で作った`OneShot`を呼ぶテスト向けの入口。
  `main`（`src/main.rs`）はqueueの位置の解決、`Generators`の1回の組み立てと注入、CLIの解析と役割による拒否、`compose`の入口か`TaskStore`のportの1回の呼び出し、JSONの出力とexit codeだけを行う。
  queueの読み取りの`compose::read_queue`は開いたqueueと`QueueReadSources`の実装を`application::queue_reads::answer`に渡す配線だけを持つ。
  `SqliteQueue`を開くのは`compose`と`infrastructure`だけ。
- レイヤーの外: `view`（`show`・`goal show`の圧縮した出力）。
  applicationは`view`を参照しない（[architecture](architecture.md)）。

利用方法は[README](../../README.md)を参照。

dagqは、依存関係を持つ開発タスクをSQLiteで管理し、着手可能なタスクをGit worktreeとsupervisorから切り離したbackgroundのsession wrapperで実行するRust runtimeである（cmuxはinboxのworkspaceに使う）。

> **予定**: cmuxはinboxだけが使う形に移る（[ADR-t1433-1](../adr/2026-10-03-t1433-1-cmux-is-used-only-by-the-inbox.md)）。
> cmuxを呼ぶのは`up`がinboxのworkspaceを開く・確かめる・閉じることとinboxのsessionの中の操作だけになり、worker・runtimeのplanner・supervisor・queue service・observer・jobはcmuxを呼ばない。
> 対話の経路（workerの`--interactive`とruntimeのplannerの対話）は廃止し（[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)）、非対話のsession wrapperはbackgroundのprocessだけで動き（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）、runtimeはinboxのterminalに打ち込まない（[ADR-t1433-5](../adr/2026-10-03-t1433-5-inbox-watch-without-typing-into-the-inbox.md)）。
> workerのsession wrapperとruntimeのplannerはbackgroundだけで動き、runもruntimeのplannerもworkspaceを開かない。
> workerの対話の経路は動かず、`add` / `edit`の`--interactive`は理由付きで拒み、workerへの文面に対話の分岐は無い。
> supervisorはlaunchdだけで常駐し、`--in-cmux`は拒む（[ADR-t1433-4](../adr/2026-10-03-t1433-4-supervisor-resides-without-cmux.md)）。
> この文書の対話の記述は、後続のtaskが実装するまでの今の姿である。

## 用語集

役割は5つで、ADR-0024の決定1で確定し、[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1が引き継いだ: supervisor、worker、planner、inbox、observer。
このうち`DAGQ_ROLE`で名乗るworkspaceを持つのはinboxだけで、cmux workspaceのtitleは`[<repo>]<role>`（[ADR-0028](../adr/0028-workspace-titles-are-repo-and-role.md)）。
workerとruntimeのplannerはworkspaceを持たず、session wrapperはbackgroundのprocessで動く（[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)）。
`up`が開くのはinboxだけで、plannerはproposalごとのオンデマンドのbackgroundのwrapperとしてruntimeが立てる（ADR-0044の決定1・6。[supervisor-lifecycle](supervisor-lifecycle/plan-planners.md#plan--planners)）。
人が開くplannerは[ADR-t1394-1](../adr/2026-10-03-t1394-1-abolish-person-planners-and-route-planning-through-inbox-requests.md)で廃止した: 人はinboxに計画を頼み、inboxが人の言葉を計画の依頼として記録し、supervisorが依頼ごとにruntimeのplannerを立てる。
`dagq plan`は何も開かず、その案内を付けて拒む。
runtimeのplannerは経路を選ばず非対話（workerと同じturn）だけで、supervisorから切り離したbackgroundのwrapperで動く（[ADR-t1394-2](../adr/2026-10-03-t1394-2-runtime-planner-route-interactive-or-headless.md)の非対話の経路。対話の経路は[ADR-t1433-2](../adr/2026-10-03-t1433-2-abolish-the-interactive-route.md)で廃止、[ADR-t1433-3](../adr/2026-10-03-t1433-3-headless-wrappers-run-only-in-the-background.md)。[runtimeのplannerの経路](supervisor-lifecycle/plan-planners.md#runtimeのplannerの経路)）。
planの検査はsupervisorが起動するheadlessのplan review jobが行い、taskを`ready`にする。
observerとreview / triageのjobはworkspaceを持たないheadlessのjobで、`dagq.toml`の`[roles.<role>]`の`provider`（runのreviewのagentのjobは`[review.subagents.<agent>]`のものが上書きする）が決めるprovider（[Actor model](supervisor-lifecycle/actor-model.md#provider)・[Review](supervisor-lifecycle/review.md#agentのjobのprovider)）で動き、observerとjobの`DAGQ_ROLE`は[Roles](supervisor-lifecycle/roles.md#actors)が持つ（[ADR-t728-1](../adr/2026-09-27-t728-1-trust-domains-actors-and-default-deny-capability-authorization.md)）。

| 用語 | 指すもの | 旧称 |
| --- | --- | --- |
| **supervisor** | runtimeの`dagq supervise`プロセス。依存が解けたtaskをclaimし、runごとにworktreeを作ってworkerのsession wrapperをbackgroundで起動し、receiptを検証し、runごとのheadlessのjob（review、triage）を起動してそのverdictで着地・差し戻し・resume・retryを行い、`needs_session`のrunをresumeし（`MAX_RESUME_ATTEMPTS`で解消しなければ人へのaskにする）、後始末をする（[ADR-0003](../adr/0003-supervisor-owns-lifecycle.md)、[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)、[ADR-0040](../adr/0040-verify-once-review-run-env-graph-stats-and-task-priority-in-claim-order.md)、[ADR-0027](../adr/0027-keep-worker-session-through-review-revise-verdict-and-merge-tree-precheck.md)、ADR-0044）。launchdのLaunchAgentとして常駐する。 | "SV"（supervisorの略） |
| **worker** | runごとにsupervisorが起動するClaude（将来はCodex）のsession。割り当てられたworktreeの中だけで作業し、commitしてreceiptを書く。判断が要るときは`worker_question`のaskを登録して止まる。 | agent session、run session |
| **planner** | goal / taskを書いてproposalとしてsubmitし、inboxが記録した計画の依頼を計画するか理由付きで却下し、follow_upのdraft taskの採否とobserverのfindingの扱いを自分で決め、goalをcloseするのは採らないdraft goalの破棄（`--verdict abandoned`）と人の言葉のときだけのsession（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0044の決定1・6）。常駐せず、proposalごとのオンデマンドのbackgroundのwrapperとturnで動き、workspaceを持たない。runtimeが立てる（revise・draft・finding・inboxからの計画の依頼。複数同時に開ける）。画面を見る人は居ない。`up` / `down` / `install`は打たない（CLIは許すが、inboxか人の`DAGQ_ROLE`の無いterminalが打つ）。runtimeのplannerは経路の選択なしに非対話（turnごとの呼び出し）だけで動く（ADR-t1394-2・ADR-t1433-2・ADR-t1433-3）。決めきれない判断は`planner_question`でinboxに上げる（ADR-t1394-1）。 | （新設） |
| **inbox** | 人に届くものすべての窓口になる常駐session（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、ADR-0044の決定6）。openなaskを人に見せてanswerを書き戻し、それ以外のattention（回答済みのask、止まったsupervisor、失敗したreview / triage、pushの失敗）を人に知らせ、人の指示があるときだけ`dagq-recover`の手順（`up` / `down`、手でのreviewと`integrate`、`recover`。workerのrunへの`run send`とplannerへの`planner send`はどれにも拒まれ、workerとplannerへの答えは`answer`で記録してsupervisorが届ける）を実行する。自分では判断しない。人が頼んだ計画は人の言葉のまま計画の依頼として記録してruntimeのplannerに移譲し（`dagq request add`、ADR-t1394-1）、結末（`proposed` / `declined` / `exhausted`）と依頼についての`planner_question`を人に伝える（手順は`dagq-inbox` skillの`reference/requests.md`）。`up`が`[<repo>]inbox`のworkspaceに`inbox_prompt`付きで開く。 | （新設） |
| **observer** | supervisorのtimerで定期起動するheadlessのjob（ADR-0044の決定4）。状態は変えない。`stats`、閉じていないfinding、直近のnoteを読み、findingの記録と更新（決定18）とfindingに紐づく`blocked`のask（決定23）だけを書く。noteとdraft goalは書かない。 | （新設） |

**退役した役割。**
ADR-0010からADR-0023までの記述に出てくる常駐のClaude Code session「メンテナー」（英字表記の役割名。`up`が`[<repo>]`＋その名のworkspaceを開き、`DAGQ_ROLE`にその名を持っていた）は、ADR-0024で退役し、その決定は[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定1が引き継いでいる。
既存のADRは書き換えないので、そこでのメンテナーの仕事は次のとおり読み替える: レビューと着地はsupervisorのreview job、失敗runの扱い（recoverしてready / cancel）はsupervisorのtriage job、resumeの上限（`MAX_RESUME_ATTEMPTS`）で解消しない`needs_session`はinbox宛てのask、継続的な監視と改善提案はobserver、人への相談とanswerに従う操作・`up` / `down` / 固定バイナリの更新は人の指示でinboxのsession（か人の`DAGQ_ROLE`の無いterminal）から行う。
`up`はそのworkspaceを開かず、queueに記録されたそのworkspaceの行を忘れる（workspace自体は人が閉じる）。

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
   session wrapper  provider
   (background)   ┌───┴───┐
          │       ▼       ▼
          │     Claude  Codex
          ▼
       Git worktree
```

テストは3層に分ける（置き場所・書き方・時間の規則は[testの制約](../development/testing.md)）。
判断は`src/`の副作用のない関数の`#[cfg(test)]`のunit testが確かめる。
SQLiteのキュー・CLI・cwdからのqueueの解決、providerをテストダブルに差し替えたsupervisorとwrapper、launchdとsignalをfakeに差し替えた`up` / `down`・`dagq inbox`・入れ替え・`install`の境界は、1つのtest binary `it`（`tests/it/main.rs`。[ADR-0078](../adr/0078-one-integration-test-binary.md)）が確かめ、unit testと合わせて行カバレッジの対象になる。
`tests/e2e.rs`（と`tests/e2e/`）は実バイナリ・実Git・実launchdで、使い捨てのrepositoryに`init`から`supervise`と`integrate`のsquash着地までのハッピーパスと境界を通す。
Claudeの代わりに、workerの非対話のturnとしてpromptどおりに変更・commit・receiptを書くstub（`tests/e2e/stub.rs`）をproviderにする。
e2eは実プロセスを要るので`#[ignore]`にし（流し方は`tests/e2e.rs`のmodule doc）、カバレッジに含めない。

タスクは`draft | submitted | ready | in_progress | completed | canceled`を持つ。`submitted`はplan review待ちで（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)の決定8）、plannerが`submit`でproposalにして出す。`ready`にするのはplan reviewの経路と、人の`ready --bypass-review`と、失敗したrunの再試行だけ。`ready`で依存先がすべて`completed`のタスクだけがschedulerの起動候補になる。詳細な実行状態はTaskRunに保存する。

supervisorはagentの完了レシート、コミット、テスト、worktreeのclean状態を確認してからrunのsession wrapperを止める。worktreeとbranchは`integrate`がmainへ着地させるまで残し、着地後に`integrate`が削除する。

### Gitの実行fileの解決

`infrastructure::git_binary::git_executable`はprocessごとの`LazyLock`で、最初のPATHから`git`の絶対pathを一度だけ解決する。
macOSで解決先が`/usr/bin/git`（Command Line Tools / Xcodeのxcrun shim）のときだけ、`/usr/bin/xcrun --find git`で選択中のdeveloper toolsの実体を求める。
絶対pathで、正規化できる実行可能な通常fileならそれを使う。
Homebrew・Linuxなどのgitには追加の探索をしない。
xcrunの起動失敗・非zero終了・30秒の探索期限・不正な出力・実体の不在や実行権限の不足では、最初に見つけたgitへfallbackする。
最初のPATHにもgitが無ければエラーになる。
解決結果（fallbackとエラーを含む）はprocessの寿命中固定で、PATHやdeveloper toolsを変えた場合は次のprocessから反映される。
