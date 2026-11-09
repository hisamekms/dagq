---
id: design-persistence
type: design
title: SQLite persistence
status: current
created: 2026-09-21
scope: persistence
related:
  - adr-t1704-1
  - adr-t1639-1
  - adr-t1394-1
  - adr-t1632-1
  - adr-t1340-1
  - adr-t807-1
  - adr-0067
  - adr-t614-2
  - adr-t876-1
  - adr-0062
  - adr-0073
  - adr-0044
  - adr-0040
  - adr-0038
  - adr-0003
  - adr-0006
  - adr-0007
  - adr-0008
  - adr-0009
  - adr-0010
  - adr-0011
  - adr-0012
  - adr-0039
  - adr-0014
  - adr-0045
  - adr-0017
  - adr-0020
  - adr-0024
  - adr-0022
  - adr-0029
  - adr-0037
  - adr-0063
  - design-domain-model
  - design-measurement
  - design-measurement-environment
---

# SQLite persistence

## 目的

SQLiteのDBはqueueの正本で、プロセス間の共有と再起動の後の復旧に使う。
stdoutやsessionの画面は正本にしない。

## 全体の流れ

```text
queue service（ADR-t3102-2の例外: DBの無い所へのinit）
        │  port（application の trait）
        ▼
SqliteQueue（src/infrastructure/）── BEGIN IMMEDIATE ──► queue.db（WAL）
        │  行を読む → domainの集約に復元 → domainのコマンド → 返った集約を UPDATE / INSERT
        │  同じトランザクションで run_events に event を書く
        ▼
serviceの読み取りの接続 ── SQLITE_OPEN_READ_ONLY（このbinaryを拒むqueueは拒み、古いschemaは読まない）
```

## 責務と境界

- 判断（遷移を許すか、どのeventを書くか、payloadの中身）はdomainが持ち、storeは読む・復元する・書く・同じトランザクションにまとめるだけを持つ（[ADR-0013](../adr/0013-layered-architecture-and-type-function-style.md)）。
- 例外は全体のグラフが要る依存の循環の検出（再帰CTE）だけで、拒むかの判断はdomainが持つ。
- 共有のSQLを流す入口は実行のport（`src/infrastructure/sql_exec/`）。
  Postgresの読み書きの入口は`src/infrastructure/postgres/`。
- Postgresの実行のportが共有のSQLの引数・値・行を写し、storeは方言ごとのSQLを持たない。
- 時刻とIDは`SqliteQueue`の`Generators`から取り、SQLの`now`で作らない。
  例外はDBの時計で判じる`write_leader`のleaseの期限（ADR-t3986-1決定4）。
- schemaの形は`migrations/*.sql`、適用と互換の判定は`src/infrastructure/schema.rs`が持つ。
- 共有のSQLは引数を型を決める形でNULLと比べ、`INSERT OR`・値との`IS` / `IS NOT`・`ifnull`を使わない（境界の検査が守る）。
- 方言の違う式の断片は`src/infrastructure/sql_dialect.rs`の関数が返す（ADR-t3982-1）。
  JSONの関数と`unixepoch()`は方言の関数のmoduleの外で綴らない。
  行の順と選び方はrowidに頼らず挿入の順の方言の関数で決め、真偽は両方のDBで同じに動く形で比べ・書く（境界の検査が守る）。
- runのログ本体・receipt・promptはDBに置かず、queueのディレクトリの`runs/<run-id>/`のファイルに置く。

## 大きなeventの本文

本文を鍵で置き・読み・消すapplicationのportはstoreのtransactionに加わらない。
閾値を超えたeventの本文だけを行を書く前に渡すので、行の鍵は保存済みの本文だけを指し、rollbackで孤児が残りうる。
後から足す欄も同じ判定を通る。
鍵を持つ行に足す欄は失われ、鍵を読み戻す経路も無い。

ローカルの既定のadapterは本文をそのまま列に残し、行・schema・読める値を変えない。
runのファイル・台帳の行・古いeventは通さない。

## 不変条件

- taskごとに未完了のrunは1つ、`integrated`のrunは1つ、queue全体で`integrating`のrunは1つ（部分UNIQUE index）。
- taskごとに開いた再計画の操作は高々1つで、保護しない操作はtaskの最新のrunにあり、保護する操作は保留したrunに残る（indexでなく書き込みのtransactionが守る）。
- eventと所属の判断は追記だけで、eventは知らないkindも落とさずに読む。

## 表

`members`: 人の登録簿。
runnerの登録とlease、imageのdigestの表: `src/domain/runner_lease.rs`・`image_publish.rs`
`ledger_omitted_points`: 台帳の写さないeventの被覆の点（[計測](measurement.md#台帳を作る係)）。
被覆と点はW・Rと同じtransactionで書き、消さない。

表ごとの役割と列の意味は、表を作るmigrationのコメントと表を持つstoreと`src/domain/`の型のdoc commentが持つ。

## 集約の読み書き

`tasks`・`goals`・`task_runs`の行はdomainの集約`Task` / `Goal` / `TaskRun`と行き来する（[domain-model](domain-model.md#集約-taskとgoal)、[集約: TaskRun](domain-model.md#集約-taskrun)）。

- 入口: task・goal・依存・claim・一覧は`src/infrastructure/task_store.rs`、runと計測はportごとの`runtime_store/`。
- 読む: 行から`TaskRecord`などを組んで`restore`で復元し、復元が拒んだ行は変換エラーになる（既定値に読み替えない）。
- 変える: 読んだ集約をdomainのコマンドに渡し、返った値を書く。
  `WHERE status=<読んだstatus>`や`AND supervisor_token=<token>`は並行の変更を見つける条件で、どの遷移を許すかの判断ではない。
- owner（ADR-t3989-1）: goalに属するtaskはownerを持たずgoalの行から読み、goalの無いtaskだけが自分のownerを持つ。
  所属を変える書き込みは同じtransactionで、goalに入れるtaskのownerを空にし、goalから外すtaskに外す前のgoalのownerを写す。
- 認可の後の変化: 計画系のコマンドは認可に使ったtaskの状態を渡し、storeはトランザクションを開いた直後に照合して、変わっていれば何も書かずに拒む（[Authorization](authorization.md)）。
  supervisor（`supervisor_needs`）と着地のintegrator（`integrator_scope`）の操作はserviceがcapabilityで認可し、taskの状態は照合しない。
- runのdomainの拒否は、トランザクションが巻き戻ってeventに残せないので、run directoryの`refusals.log`に1行で残す。

## asks

`asks`は人の判断を要る相談を1行で持つ（[ADR-0022](../adr/0022-ask-answer-inbox-planner-and-landing-on-doubt.md)、[ask](supervisor-lifecycle/ask.md)）。
入口は`src/infrastructure/asks.rs`と`runtime_store/ask_store.rs`、行の型は`src/domain/`の`Ask`と`NewAsk`。

未回答のaskはcloseできない。
行とeventの整合・回答の配送印・同じaskの判定は`src/infrastructure/asks.rs`のdoc commentが持つ。

### draft planners（`draft_planners.rs`）

runtimeやjobが作ったdraftに立てるplannerの記録（[draft planners](supervisor-lifecycle/draft-planners.md)、[domain-model](domain-model.md#draft-planners)）。
入口は`src/infrastructure/draft_planners.rs`（port `DraftPlannerStore`）。

### planning requests（`plan_requests.rs`）

人がinboxに頼んだ計画の依頼と、依頼ごとに立てるruntimeのplanner（ADR-t1394-1、[inboxからの計画の依頼](supervisor-lifecycle/plan-planners.md#inboxからの計画の依頼)）。
入口は`src/infrastructure/plan_requests.rs`。

## findings

`findings`はobserverが見つけた問題を1行で持つ（[ADR-0044](../adr/0044-findings-proposals-from-findings-and-quiet-observer.md)、[Observer](supervisor-lifecycle/observer.md)）。
入口は`src/infrastructure/findings.rs`、判断と一覧の絞りは`src/domain/finding.rs`。

## Runtime ownership

書き込みの係のlifecycleの入口は`infrastructure::queue_service::serve`と`ServiceWriteLine`。
serviceは起動で係を引き継ぎ、予定した停止で列を空にしてから手放し、`not_leader`を受けた係は終わり、lockのファイルを証にできるローカルのSQLiteは期限を待たない。

runの所有・supervisorの登録・runのファイルの約束（[ADR-0007](../adr/0007-run-level-leases-parallel-execution.md)、[ADR-0039](../adr/0039-adopt-stale-lease-of-live-wrapper-and-renew-own-stale-lease.md)）。
入口は`src/infrastructure/runtime_store/coordination.rs`と`transitions.rs`、`recovery.rs`。

- leaseは実行中のrunの所有で、実行の記録と揮発する所有権を混ぜないよう`task_runs`の列にせず別の表にする。
- claimはrun・`supervisor_token`・lease行を1トランザクションで作り、所有者のないclaimed runを作らない。
- runを変える書き込みは同じトランザクションで自分のlease行のheartbeatを更新し（`renew_lease`）、0行なら何も書かずに拒む。
  heartbeatの古さは拒む理由にしない（hostのsleepの後も、引き継がれるまでは自分のもの）。
- 動いているrunのleaseを奪うのは引き継ぎ（`adopt_run`）だけで、他のtokenのstaleなleaseでwrapperが生きているか終了を記録したrunだけを対象にする。
  自分の更新と引き継ぎはどちらもlease行への書き込みなので、SQLiteが直列化して片方だけが通る。
- `integrate`は登録せずにleaseだけを持つので、`run_leases.token`から`supervisors`へ外部キーを張らない。
- `supervisors`の行はそのプロセスの性質を持ち、行と寿命を共にする。
  登録の削除は停止のeventと不可分で、決まった経路だけが行う（`coordination.rs`）。
  掃除の条件は`src/application/supervise/host_ops/sweep.rs`の`prune_left_supervisors`、登録の値の寿命は`coordination.rs`のdoc commentが持つ。
- `recover`だけがtokenなしで未完了のrunを止め、同じトランザクションでleaseとプロセスを再検査する。
- 着地（`integrate`）は自分のtokenでlease行を持ち、`integrating`から出る遷移はどれもそのlease行を要する。
  Gitの操作はトランザクションの外で行い、mainを進める前のエラーではrunを元のstatusに戻す。
- receiptの内容は読んだ時点でevent（`validation_finished`・`integration_receipt`）に写すので、worktreeとrun dirが消えた後も`show`で辿れる。
  `needs_session`をsessionが解消した後のreceiptは、そのrunの最後の`integration_receipt`が持つ。
- backendの呼び出しの失敗とsessionの終了の依頼は、runのstatusを変えずeventだけで表す（[backendの呼び出しの失敗](supervisor-lifecycle/backend-call-failures.md)、[receiptとsessionの終わり](supervisor-lifecycle/headless-worker.md#receiptとsessionの終わり)）。
- 時刻: 1つの操作は`Clock`を1回読んで使い回し、leaseの鮮度の判定の基準もその時刻にする。

### queue service

queue DBを開くのはqueue service（[Queue service](queue-service.md)）と[その例外](queue-service.md#dbを開く経路)だけで、SQLiteのトランザクションとleaseの規則で並ぶ。
送り直される結果の記録のeventは最初の送信の識別子と`event_records`で結んで同じトランザクションで書き、識別子ごとに1つのeventにする（ADR-t3102-3決定5）。

## session steps

`session_steps`はSessionStepStore（[計測](measurement.md#ssotとビュー)）の表で、sessionのstepを1行ずつ持つ。
入口は`runtime_store/session_steps.rs`、型は`domain::session_step`。

保持で消したseqを配送の抜けと数えない。
保持と再配送の整合の入口は`runtime_store/session_steps.rs`のdoc comment。

## runの資源

`run_resources`はsupervisorが外から測ったrunの資源の窓を持ち、保持で消さない（[実行環境と資源](measurement-environment.md)）。

## follow-upの所属の判断

follow-upの所属の判断・訂正とdraft / readyの所属の変更は同じ`BEGIN IMMEDIATE`のトランザクションで行う。
行・遷移・必須の情報・旧schemaの読み方は[所属の判断](follow-up-membership.md)、入口は`src/infrastructure/follow_up_membership.rs`。

## Transactions and constraints

- 両adapterの通常の書き込みは入口でfenceし、係と世代が一致しなければ何も書かず`not_leader`で拒む。
  期限は条件に入れない。
- 係の引き継ぎ・延長・解放はfenceせず、共有のSQLの条件付きの更新を流す。

- ロックの待ちは`QUEUE_BUSY_TIMEOUT`までで、超えればerrorを返す。
  supervisorのheartbeatは`QueueBusy`を次の間隔で書き直す（[supervise](supervisor-lifecycle/supervise.md#起動とheartbeat)）。

queue DBのtransactionの入口は`infrastructure::sqlite::WriteTx`（境界の検査は`src/context_boundaries/write_tx.rs`）。

- claimは候補の選択・taskの更新・runの作成・event・lease行を1つのトランザクションにまとめ、同時のclaimは別々のtaskを取る。
  holdの行と`task_held` / `task_released`も1つのトランザクションで書き、claim・resume・復旧jobの開始と結果の適用はそれぞれのトランザクションでholdを読むので、先に確定した側だけが効く。
- 外部キーと依存の複合主キーはDBに置き、自己依存はdomainが拒む。
- NULLになる列を含む主キーはSQLiteではその行の一意を保たないので、その一意は書く関数がトランザクションの中で読んでから検査する。
- taskの詳細と一覧は1つのread transactionで読むので、task・run・eventのスナップショットが揃う。

### CHECK制約を使わない

schemaが安定したと人が決めるまで、SQLiteのCHECK制約を使わない（[ADR-t876-1](../adr/2026-09-28-t876-1-no-sqlite-check-constraints-until-schema-is-stable.md)）。

- 対象はCHECKだけで、NOT NULL・UNIQUE・主キー・外部キー・DEFAULTはDBに残す。
- 規則の置き場所: 値の一覧はdomainの型（`string_enum!`）、型で表せない規則は書き込みのportが書く前に検査し、破れていれば書かずにerrorにする（`src/domain/write_rules.rs`）。
- 読むとき: askとeventのkindと、`src/domain/write_rules.rs`のmodule docが挙げる列は知らない値を寛容に読む（区分の追加を互換にするため）。
  それ以外の列で規則の外の値を読むと、その読み込みはerrorで止まり（fail closed）、DBの行はその場で直さない。
- 値を足すとき: kindの列への追加はmigrationを要さない。
  読む側がfail closedの列への追加は、古いバイナリが読めないので非互換の宣言のmigrationで下限を上げる（表は作り直さない）。
- kindの規則（[ADR-0073](../adr/0073-kind-additions-are-compatible.md)）: 書き込み口はkindを`EventKind`で受け、kindに結び付く規則は書く前に検査する。
  queueのeventのkindは`EventKind::is_queue`にも足す。

## Queue location

queueはrepositoryごとに1つで、場所は`src/infrastructure/location.rs`の`QueueLocation`が決める（[ADR-0006](../adr/0006-queue-per-repository.md)）。

```text
$XDG_DATA_HOME/dagq/<hash>/          未設定・空・相対pathなら $HOME/.local/share
  queue.db                           SQLite（WALの -wal / -shm も隣に置かれる）
  repository                         束縛先のGit common directory（人向けの逆引き）
  runs/<run-id>/                     prompt、runner、worktree/、receipt、refusals.log、ログなどrunのファイル
  logs/                              processごとのJSON Lines、自動更新の出力、launchd.log、rebind.jsonl
  service/                           queue serviceのsocket・lock・token
  backups/                           非互換のmigrateの前の複製
~/Library/LaunchAgents/com.dagq.<hash>.plist   upが書くsupervisorのLaunchAgent
```

- `<hash>`はcanonicalizeしたGit common directoryのhashなので、symlink経由やworktreeからでも同じqueueになる。
- repositoryを移すとhashが変わり、新しいqueueに解決される。
  付け替えは明示の`rebind`だけが行う（[ADR-0020](../adr/0020-rebind-queue-to-a-moved-repository.md)、[コマンドをまたぐ約束](supervisor-lifecycle/commands.md)）。
- queueのディレクトリは丸ごと移せる: runのpathは開いた場所から解決し直し、worktreeのGitの管理情報は`integrate`が直す（[ADR-0017](../adr/0017-resolve-run-paths-from-the-queue-directory.md)）。
- `logs/`の書式と消す時期は`infrastructure::telemetry`、`planners/`・`plan-reviews/`・`observer/`・`backups/`は`[retention]`（ADR-t697-1）。
- storeの選択の入口は`compose::refuse_unusable_store`で、どのコマンドもqueueを開く前に通る。
- storeの種類・接続先は配置設定が持つ。
- 既定はSQLite、使えないstoreなら代用せず止まる。
- Postgres接続は明示の平文指定が無ければTLSでサーバの証明書を確かめ、平文に落ちない。

## Database setup and migrations

入口は`src/infrastructure/schema.rs`（`MIGRATIONS`と互換の判定）と`src/infrastructure/sqlite.rs`の`SqliteQueue`。
migrationの登録と順序は[ADR-0067](../adr/0067-migrations-are-listed-by-build-and-renumbered-on-landing.md)、互換の宣言と判定は`schema.rs`のdoc commentが持つ。
migrationを足すtaskの規則（リリース済みは変えない）は[migrations.md](../development/migrations.md)が持つ。

- queueを開いただけではmigrateしない。
  migrationを適用するのは空のファイルへの`init`と、`dagq migrate`の依頼か[`up`](supervisor-lifecycle/up-down.md)・[install](supervisor-lifecycle/install.md#install)・[自動更新](supervisor-lifecycle/auto-update.md#auto-update)の入れ替えの口（受け付けの前）のqueue serviceだけ（[ADR-0045](../adr/0045-build-identifier-explicit-migrate-schema-compat-handoff-and-auto-update.md)、ADR-t3102-2決定2）。
  serviceを起動できるのはqueueのbinary（DBの隣の`queue-binary.json`が名指すpath。`init`は作ったbinaryを、installは置いたpathを名指す）だけ（決定3）。
  記録の無いqueueを名指すのはinstallか、serviceの動いていないときの最初の起動で、動いているserviceを止める・入れ替える起動はinstallのgrantが無ければ断る。
- open: 下限がバイナリより新しければ止まり、`user_version`がバイナリより古ければ`dagq migrate`を案内して止まる。
  バイナリより新しくても下限以下なら動く。
  このため`INSERT`は必ず列を列挙し、行は列名で読む。
  claimのときに写したバイナリ（`runs/<id>/runner`）も互換のmigrationの後に動き続ける。
- 表の作り直し: 全ての列を列挙してrowidと`sqlite_sequence`も写し（消した行のIDを再び払い出さない）、`foreign_keys`はrunnerがトランザクションの外でOFFにしてcommitの前に`pragma_foreign_key_check`を確かめる。
  `run_events`を作り直すときは、actorの列・検索のtrigger・`run_events`のindexも作り直す。
- 落とし穴: `run_events`の式のindex（`events_by_opened_event`）の式の列は、queryが同じ式でaffinityの掛からない値（bindした値か`+o.id`）と比べるときだけ使われ、先頭列の`kind`だけでも選ばれうる。
  `run_id IS NULL`は`+run_id`と書いて`events_by_run`を避けるが、`goal_id IS NULL`も絞ると`events_by_goal`を辿りうるので、lockの下は`INDEXED BY events_by_kind`（か3列とも`+`）で固定する。

`run_events`の読みのplanはlibのtestが入口で検査し、例外は理由つきで一覧に載せる。

## 全文検索（search）

`dagq search`の索引（[ADR-0063](../adr/0063-full-text-search-related-with-mentions-and-search-strength-and-duplicate-of.md)決定1〜3）。
計画の記録と独立したport `SearchIndex`を通す。
SQLite adapterは`src/infrastructure/search.rs`、索引とtriggerは`migrations/0026_search.sql`、問い合わせの解釈と抜粋は`src/domain/search.rs`。

- 索引はSQLiteではtriggerが同じトランザクションで保ち、索引を知らない古いバイナリの書き込みにも追いつく。
  `tasks` / `goals` / `run_events`を作り直すmigrationはtriggerを作り直す。
  Postgresはtriggerを置かず、元の行を書くadapterが同じトランザクションで保つ（ADR-t3986-1決定6）。

## 関連（related）

`dagq related TASK`（ADR-0063決定4）は表を足さず、既存の表と`search_index`を読む。
SQLite adapterは`src/infrastructure/related.rs`、手がかりと点数と重みは`src/domain/related.rs`。

## CIの見張り

[ADR-t1920-1](../adr/2026-10-06-t1920-1-supervisor-watches-main-ci-keeps-known-failures-and-files-fixes-through-findings.md)と[CI watch](supervisor-lifecycle/ci-watch.md)。
入口は`src/infrastructure/ci_watch_store.rs`。
